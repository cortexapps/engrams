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

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::outbox::{OutboxKind, OutboxRow};
use engram_core::types::session::{SessionMode, SessionSpec};
use engram_core::SessionId;

async fn connect() -> Option<Arc<dyn MetadataStore>> {
    let url = std::env::var("ENGRAM_TEST_DATABASE_URL").ok()?;
    let store = engram_postgres::PostgresStore::connect(&url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    Some(Arc::new(store))
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
