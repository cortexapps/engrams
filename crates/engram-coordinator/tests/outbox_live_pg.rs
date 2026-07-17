//! Live-Postgres tests for the ADR 0073 phase-2 `session_outbox` store
//! layer: idempotent enqueue, due-scan, per-session oldest-first order,
//! delivered/ack/defer transitions, the edit/dequeue-of-undelivered
//! arms, and the binding-epoch mint (0083). This pins the SQL the
//! outbox delivery driver stands on — the driver's control flow is
//! thin over these operations plus `ensure_active`, which the e2e
//! stack exercises end to end.
//!
//! Coverage note (investigate-never-delete): the retired host-side
//! replay-buffer test (`harness_command_redelivery.rs`) asserted that a
//! prompt lost to a connection bounce is re-sent; that at-least-once
//! property now lives in these rows (`not_before` re-arms delivery
//! after `outbox_mark_delivered` with no ack) — asserted here in
//! `delivered_row_rearms_after_ack_timeout`.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. Wired into ci.yml's Postgres-gated list.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::outbox::{tool_result_outbox_id, OutboxKind, OutboxRow};
use engram_core::types::session::{SessionMode, SessionSpec};
use engram_core::SessionId;

async fn connect() -> Option<Arc<dyn MetadataStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
}

async fn seed_session(meta: &Arc<dyn MetadataStore>) -> SessionId {
    meta.create_session(SessionSpec {
        image: "localhost:5001/demo:outbox-test".into(),
        mode: SessionMode::Agent,
    })
    .await
    .expect("create session")
}

fn prompt_row(session_id: SessionId, prompt_id: &str, text: &str) -> OutboxRow {
    OutboxRow {
        prompt_id: prompt_id.into(),
        session_id,
        kind: OutboxKind::Prompt,
        payload: serde_json::json!({ "text": text }),
        created_at: Utc::now(),
        attempts: 0,
        not_before: Utc::now(),
        delivered_at: None,
        acked_at: None,
    }
}

fn tool_result_row(session_id: SessionId, tool_call_id: &str) -> OutboxRow {
    OutboxRow {
        prompt_id: tool_result_outbox_id(session_id, tool_call_id),
        session_id,
        kind: OutboxKind::ToolResult,
        payload: serde_json::json!({
            "tool_call_id": tool_call_id,
            "result_json": r#"{"ok":true}"#,
        }),
        created_at: Utc::now(),
        attempts: 0,
        not_before: Utc::now(),
        delivered_at: None,
        acked_at: None,
    }
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn enqueue_is_idempotent_on_prompt_id() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;
    let row = prompt_row(sid, &format!("p-{sid}"), "hello");
    meta.outbox_enqueue(&row).await.expect("first enqueue");
    // A caller retry (same prompt_id, different text) must be a no-op,
    // not a duplicate row and not an overwrite.
    let mut retry = row.clone();
    retry.payload = serde_json::json!({ "text": "retry text" });
    meta.outbox_enqueue(&retry).await.expect("retry enqueue");
    let next = meta
        .outbox_next_due(sid)
        .await
        .expect("next_due")
        .expect("row due");
    assert_eq!(next.prompt_id, row.prompt_id);
    assert_eq!(next.payload["text"], "hello", "first write wins");
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn tool_result_event_and_outbox_commit_together() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;
    let row = tool_result_row(sid, "call-1");

    let idx = meta
        .append_session_event_and_outbox(
            sid,
            "tool_result_submitted",
            serde_json::json!({ "tool_call_id": "call-1" }),
            &row,
        )
        .await
        .expect("append event and outbox");

    let events = meta
        .list_session_events_since(sid, -1, 10)
        .await
        .expect("list events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].idx, idx);
    assert_eq!(events[0].kind, "tool_result_submitted");
    assert_eq!(
        meta.outbox_next_due(sid)
            .await
            .expect("next due")
            .expect("tool result row")
            .prompt_id,
        row.prompt_id,
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn outbox_identity_conflict_rolls_back_tool_result_event() {
    let Some(meta) = connect().await else { return };
    let owner_sid = seed_session(&meta).await;
    let target_sid = seed_session(&meta).await;
    let row = tool_result_row(target_sid, "call-1");

    // Simulate a pre-existing globally keyed command owned by another session.
    // The atomic completion path must reject it and leave no visible event.
    meta.outbox_enqueue(&prompt_row(owner_sid, &row.prompt_id, "collision"))
        .await
        .expect("seed colliding command");
    let result = meta
        .append_session_event_and_outbox(
            target_sid,
            "tool_result_submitted",
            serde_json::json!({ "tool_call_id": "call-1" }),
            &row,
        )
        .await;
    assert!(result.is_err(), "cross-session collision must be rejected");
    assert!(
        meta.list_session_events_since(target_sid, -1, 10)
            .await
            .expect("list target events")
            .is_empty(),
        "the event must roll back with the rejected outbox insert",
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn conflicting_retry_payload_rolls_back_tool_result_event() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;
    let row = tool_result_row(sid, "call-1");
    meta.outbox_enqueue(&row).await.expect("seed first result");

    let mut conflicting = row.clone();
    conflicting.payload["result_json"] = serde_json::json!(r#"{"ok":false}"#);
    let result = meta
        .append_session_event_and_outbox(
            sid,
            "tool_result_submitted",
            serde_json::json!({ "tool_call_id": "call-1" }),
            &conflicting,
        )
        .await;
    assert!(result.is_err(), "a conflicting retry must be rejected");
    assert!(
        meta.list_session_events_since(sid, -1, 10)
            .await
            .expect("list events")
            .is_empty(),
        "the rejected retry must not publish a misleading result event",
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn per_session_order_is_created_at_and_head_blocks() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;
    let a = prompt_row(sid, &format!("a-{sid}"), "first");
    let mut b = prompt_row(sid, &format!("b-{sid}"), "second");
    b.created_at = a.created_at + chrono::Duration::milliseconds(5);
    meta.outbox_enqueue(&a).await.expect("enqueue a");
    meta.outbox_enqueue(&b).await.expect("enqueue b");

    let head = meta.outbox_next_due(sid).await.unwrap().unwrap();
    assert_eq!(head.prompt_id, a.prompt_id, "oldest first");

    // Ack the head; b becomes the head.
    assert!(meta.outbox_ack(&a.prompt_id).await.expect("ack a"));
    let head = meta.outbox_next_due(sid).await.unwrap().unwrap();
    assert_eq!(head.prompt_id, b.prompt_id);

    // Acks are at-least-once: a second ack is Ok(false), not an error.
    assert!(!meta.outbox_ack(&a.prompt_id).await.expect("re-ack a"));
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn delivered_row_rearms_after_ack_timeout() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;
    let row = prompt_row(sid, &format!("re-{sid}"), "redeliver me");
    meta.outbox_enqueue(&row).await.expect("enqueue");

    // Relay handoff with a sub-second ack timeout: the row leaves the
    // due set…
    meta.outbox_mark_delivered(&row.prompt_id, Duration::from_millis(300))
        .await
        .expect("mark delivered");
    assert!(
        meta.outbox_next_due(sid).await.unwrap().is_none(),
        "not due while awaiting ack",
    );
    // …and re-arms by itself when no confirming event lands (the
    // harness_command_redelivery property, durably).
    tokio::time::sleep(Duration::from_millis(400)).await;
    let re = meta.outbox_next_due(sid).await.unwrap().expect("re-armed");
    assert_eq!(re.prompt_id, row.prompt_id);
    assert_eq!(re.attempts, 1, "attempts bumped by the delivery");
    assert!(re.delivered_at.is_some());

    // The due-session scan sees it too.
    let due = meta.outbox_due_sessions().await.expect("due sessions");
    assert!(due.contains(&sid));

    // A deferred row leaves the due set again.
    meta.outbox_defer(&row.prompt_id, Duration::from_secs(3600))
        .await
        .expect("defer");
    assert!(meta.outbox_next_due(sid).await.unwrap().is_none());

    // Defers count as attempts too — `failure_backoff(attempts)` only
    // grows if a row failing BEFORE the forward (ensure_active error,
    // NotFound) bumps the counter; it used to sit at the floor backoff
    // forever and read as attempts=0 in every investigation.
    meta.outbox_defer(&row.prompt_id, Duration::ZERO)
        .await
        .expect("re-defer to now");
    let re = meta.outbox_next_due(sid).await.unwrap().expect("due again");
    assert_eq!(
        re.attempts, 3,
        "each defer bumps attempts (1 delivery + 2 defers)"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn undelivered_rows_are_editable_and_dequeueable_delivered_are_not() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;
    let row = prompt_row(sid, &format!("ed-{sid}"), "original");
    meta.outbox_enqueue(&row).await.expect("enqueue");

    // Undelivered: type-ahead edit rewrites the text in PG.
    assert!(meta
        .outbox_update_prompt_text(&row.prompt_id, "edited")
        .await
        .expect("edit"));
    let head = meta.outbox_next_due(sid).await.unwrap().unwrap();
    assert_eq!(head.payload["text"], "edited");

    // Once delivered, the PG arms refuse — the harness queue owns it.
    meta.outbox_mark_delivered(&row.prompt_id, Duration::from_secs(30))
        .await
        .expect("deliver");
    assert!(!meta
        .outbox_update_prompt_text(&row.prompt_id, "too late")
        .await
        .expect("edit after delivery"));
    assert!(!meta
        .outbox_delete_undelivered(&row.prompt_id)
        .await
        .expect("dequeue after delivery"));

    // A second, undelivered row deletes cleanly.
    let mut second = prompt_row(sid, &format!("dq-{sid}"), "dequeue me");
    second.created_at = Utc::now();
    meta.outbox_enqueue(&second).await.expect("enqueue second");
    assert!(meta
        .outbox_delete_undelivered(&second.prompt_id)
        .await
        .expect("dequeue undelivered"));
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn binding_epoch_mints_monotonically_per_session() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;
    assert_eq!(
        meta.current_binding_epoch(sid).await.expect("current"),
        0,
        "fresh session starts at 0 (never minted)",
    );
    let e1 = meta.mint_binding_epoch(sid).await.expect("mint 1");
    let e2 = meta.mint_binding_epoch(sid).await.expect("mint 2");
    assert_eq!((e1, e2), (1, 2));
    assert_eq!(meta.current_binding_epoch(sid).await.expect("current"), 2);
    // Unknown session: NotFound, never a silent 0.
    assert!(meta.mint_binding_epoch(SessionId::new()).await.is_err());
}
