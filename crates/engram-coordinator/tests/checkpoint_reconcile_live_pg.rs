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
    assert!(
        deleted.contains(&ancient.id),
        "the ancient mid-chain row must be pruned and its id returned for blob cleanup",
    );

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

/// ADR 0028 A.log: rung-1 rewind tombstones the post-cursor span
/// (kept for audit, flagged), bumps the recovery epoch, extracts
/// surviving outside-world side-effects, and stamps the new epoch on
/// subsequent events. A second rewind to the same cursor is a no-op.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn rung1_rewind_tombstones_epochs_and_surfaces_side_effects() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    let cursor = meta
        .append_session_event(
            session_id,
            "agent_message",
            serde_json::json!({"text": "before"}),
        )
        .await
        .expect("append e0");
    // Post-checkpoint span: a message + a PR (a surviving side-effect).
    meta.append_session_event(
        session_id,
        "agent_message",
        serde_json::json!({"text": "after"}),
    )
    .await
    .expect("append e1");
    meta.append_session_event(
        session_id,
        "integration_asset",
        serde_json::json!({
            "provider": "forge",
            "asset_kind": "pull_request",
            "surface": "asset",
            "data": {"number": 7},
            "fetchable": {"kind": "external", "url": "https://github.com/x/y/pull/7"},
        }),
    )
    .await
    .expect("append PR");

    let summary = meta
        .rewind_session_to_cursor(session_id, cursor)
        .await
        .expect("rewind");
    assert_eq!(
        summary.rolled_back, 2,
        "the two post-cursor events tombstoned"
    );
    assert_eq!(summary.recovery_epoch, 1, "epoch bumped 0 → 1");
    assert_eq!(summary.through_idx, cursor);
    assert_eq!(
        summary.surviving_side_effects.len(),
        1,
        "the opened PR is a surviving side-effect",
    );
    assert!(summary.surviving_side_effects[0].contains("pull/7"));

    // Replay carries the rewind flags, all rows retained (audit).
    let events = meta
        .list_session_events_since(session_id, -1, 1000)
        .await
        .expect("replay");
    let e0_row = events.iter().find(|e| e.idx == cursor).expect("e0 present");
    assert!(e0_row.rewound_at.is_none(), "pre-cursor event stays live");
    let rolled = events
        .iter()
        .filter(|e| e.idx > cursor && e.rewound_at.is_some())
        .count();
    assert_eq!(
        rolled, 2,
        "both post-cursor events are tombstoned but retained"
    );

    // A new event after the rewind carries the bumped epoch.
    let after_idx = meta
        .append_session_event(
            session_id,
            "agent_message",
            serde_json::json!({"text": "resumed"}),
        )
        .await
        .expect("append post-rewind");
    let events = meta
        .list_session_events_since(session_id, -1, 1000)
        .await
        .expect("replay 2");
    let after = events
        .iter()
        .find(|e| e.idx == after_idx)
        .expect("post-rewind present");
    assert_eq!(
        after.recovery_epoch, 1,
        "post-rewind events carry the new epoch"
    );
    assert!(after.rewound_at.is_none());

    // Rewinding to a cursor with nothing live past it is a no-op
    // (no epoch bump) — the checkpoint-was-the-head case. Use the
    // newest live event as the cursor.
    let noop = meta
        .rewind_session_to_cursor(session_id, after_idx)
        .await
        .expect("no-op rewind");
    assert_eq!(
        noop.rolled_back, 0,
        "rewinding to the head rolls back nothing"
    );
    assert_eq!(
        noop.recovery_epoch, 0,
        "no-op rewind returns default (no bump)"
    );

    // But re-rewinding to the ORIGINAL cursor DOES roll back the
    // post-recovery work appended since (idx after_idx > cursor) and
    // bumps the epoch again — re-rewinding to an old point is not a
    // no-op, it discards everything after it.
    let re = meta
        .rewind_session_to_cursor(session_id, cursor)
        .await
        .expect("re-rewind to old cursor");
    assert_eq!(
        re.rolled_back, 1,
        "the post-recovery event (idx > cursor, still live) is rolled back",
    );
    assert_eq!(re.recovery_epoch, 2, "epoch bumps again: 1 → 2");
}
