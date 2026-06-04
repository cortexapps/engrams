//! Live-Postgres integration tests for ADR 0028 Fix A's metadata
//! surface: `snapshots.events_cursor` (migration 0053 + the A.log
//! coherence triple), the `latest_event_idx_at_or_before` cursor
//! resolver, `record_snapshot`'s idempotent re-record (the heartbeat
//! reconciler's contract), and the checkpoint-retention prune.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. CI wires this into the
//! Postgres-gated-ignored lane alongside `eviction_live_pg`.

use std::sync::Arc;

use chrono::{Duration as ChronoDuration, Utc};
use engram_core::traits::MetadataStore;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::{SandboxId, SessionId, SnapshotId};

async fn pg() -> Option<Arc<dyn MetadataStore>> {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skipping: ENGRAM_TEST_DATABASE_URL not set. Run with `just db-up` first; \
                 default URL is postgres://engram:engram@localhost:5435/engram",
            );
            return None;
        }
    };
    let store = engram_postgres::PostgresStore::connect(&database_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    Some(Arc::new(store))
}

async fn seed_active(meta: &Arc<dyn MetadataStore>) -> (SessionId, SandboxId) {
    let id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:warm-ckpt-test".into(),
            mode: SessionMode::Agent,
            user_id: None,
        })
        .await
        .expect("create");
    let sandbox = SandboxId::new();
    meta.assign_session_sandbox(id, Some(sandbox))
        .await
        .expect("bind sandbox");
    meta.transition_session(id, SessionState::Created)
        .await
        .expect("pending->created");
    meta.transition_session(id, SessionState::Active)
        .await
        .expect("created->active");
    (id, sandbox)
}

fn checkpoint_row(
    session_id: SessionId,
    created_at: chrono::DateTime<Utc>,
    events_cursor: Option<i64>,
) -> SnapshotRecord {
    SnapshotRecord {
        id: SnapshotId::new(),
        session_id: Some(session_id),
        host_id: None,
        image_version: "ckpt-test".into(),
        size_bytes: 1024,
        created_at,
        last_accessed_at: created_at,
        disk_manifest: None,
        memory_manifest: None,
        recoverable: true,
        aux_bundles: vec![],
        events_cursor,
    }
}

/// events_cursor round-trips through record + read, and the
/// reconciler's idempotent re-record never clobbers a resolved cursor
/// with NULL (the COALESCE in record_snapshot's upsert).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn events_cursor_round_trips_and_survives_null_rerecord() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    let mut row = checkpoint_row(session_id, Utc::now(), Some(42));
    meta.record_snapshot(row.clone()).await.expect("record");

    let back = meta
        .latest_snapshot_for_session(session_id)
        .await
        .expect("lookup")
        .expect("row present");
    assert_eq!(back.events_cursor, Some(42), "cursor must round-trip");

    // Idempotent re-record with no cursor (e.g. a reconciler ingest
    // racing the eviction pipeline's earlier record) must NOT erase
    // the resolved value.
    row.events_cursor = None;
    meta.record_snapshot(row.clone()).await.expect("re-record");
    let back = meta
        .latest_snapshot_for_session(session_id)
        .await
        .expect("lookup")
        .expect("row present");
    assert_eq!(
        back.events_cursor,
        Some(42),
        "NULL re-record must not clobber a resolved cursor",
    );

    // A re-record WITH a cursor (the reconciler resolving what the
    // dead coord never did) does take effect.
    row.events_cursor = Some(99);
    meta.record_snapshot(row).await.expect("re-record 2");
    let back = meta
        .latest_snapshot_for_session(session_id)
        .await
        .expect("lookup")
        .expect("row present");
    assert_eq!(back.events_cursor, Some(99));
}

/// `latest_event_idx_at_or_before` resolves the pause-instant cursor
/// from real `session_events` rows.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn latest_event_idx_resolves_against_real_events() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    // No further events beyond whatever the transitions emitted —
    // resolve "as of now" and "as of long ago".
    let now_cursor = meta
        .latest_event_idx_at_or_before(session_id, Utc::now())
        .await
        .expect("resolve now");
    let ancient_cursor = meta
        .latest_event_idx_at_or_before(session_id, Utc::now() - ChronoDuration::days(365))
        .await
        .expect("resolve ancient");
    assert_eq!(
        ancient_cursor, None,
        "a cursor before any event must be None (before-everything)",
    );
    // The seed transitions may or may not have persisted events
    // depending on the emit path; if any exist, "now" must see them.
    if let Some(idx) = now_cursor {
        assert!(idx >= 0);
    }
}

/// Retention: latest-per-session always survives; in-window history
/// survives; older rows are pruned; template snapshots
/// (session_id IS NULL) are exempt.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn prune_keeps_latest_and_window_drops_aged_history() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;
    let now = Utc::now();

    // Chain: ancient (2 days), mid (2 hours), latest (now).
    let ancient = checkpoint_row(session_id, now - ChronoDuration::days(2), None);
    let mid = checkpoint_row(session_id, now - ChronoDuration::hours(2), None);
    let latest = checkpoint_row(session_id, now, None);
    for r in [&ancient, &mid, &latest] {
        meta.record_snapshot(r.clone()).await.expect("record");
    }
    // A second session whose ONLY checkpoint is ancient — latest-per-
    // session protection must keep it despite its age.
    let (other_session, _s) = seed_active(&meta).await;
    let other_only = checkpoint_row(other_session, now - ChronoDuration::days(30), None);
    meta.record_snapshot(other_only.clone())
        .await
        .expect("record other");
    // A template snapshot (session_id NULL), ancient — exempt.
    let mut template = checkpoint_row(session_id, now - ChronoDuration::days(30), None);
    template.session_id = None;
    meta.record_snapshot(template.clone())
        .await
        .expect("record template");

    let deleted = meta
        .prune_session_snapshots(ChronoDuration::hours(24))
        .await
        .expect("prune");
    assert!(deleted >= 1, "the ancient mid-chain row must be pruned");

    let ids: Vec<_> = meta
        .list_snapshots_for_session(session_id)
        .await
        .expect("list")
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert!(!ids.contains(&ancient.id), "aged-out history pruned");
    assert!(ids.contains(&mid.id), "in-window history kept");
    assert!(ids.contains(&latest.id), "latest kept");

    let other_ids: Vec<_> = meta
        .list_snapshots_for_session(other_session)
        .await
        .expect("list other")
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert!(
        other_ids.contains(&other_only.id),
        "a session's ONLY (latest) checkpoint survives regardless of age",
    );

    let template_back = meta
        .get_snapshot(template.id)
        .await
        .expect("get template")
        .is_some();
    assert!(
        template_back,
        "template snapshots are exempt from retention"
    );
}
