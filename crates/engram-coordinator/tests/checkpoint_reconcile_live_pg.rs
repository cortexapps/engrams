//! Live-Postgres integration tests for ADR 0028 Fix A's metadata
//! surface: `snapshots.events_cursor` (migration 0053 + the A.log
//! coherence triple), the `latest_event_idx_at_or_before` cursor
//! resolver, `record_snapshot`'s idempotent re-record (the heartbeat
//! reconciler's contract), and the checkpoint-retention prune.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. CI wires this into the
//! Postgres-gated-ignored lane alongside `eviction_live_pg`.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use engram_core::types::BindingDisposition;
use std::sync::Arc;

use chrono::{Duration as ChronoDuration, Utc};
use engram_core::traits::MetadataStore;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::Capability;
use engram_core::types::EnabledImage;
use engram_core::{SandboxId, SessionId, SnapshotId};
use uuid::Uuid;

async fn pg() -> Option<Arc<dyn MetadataStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
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
    // 0108: bind via the production fused path, never on a Pending row.
    meta.transition_session_created(id, sandbox)
        .await
        .expect("pending->created");
    meta.transition_session(id, SessionState::Active, BindingDisposition::Retain)
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
        fc_snapshot_version: None,
    }
}

/// A per-image base/template capture (`session_id IS NULL`) — the row shape
/// `prune_orphan_base_snapshots` reaps once it is no longer referenced by an
/// enabled image.
fn base_row(created_at: chrono::DateTime<Utc>) -> SnapshotRecord {
    SnapshotRecord {
        id: SnapshotId::new(),
        session_id: None,
        host_id: None,
        image_version: "base-reaper-test".into(),
        size_bytes: 1024,
        created_at,
        last_accessed_at: created_at,
        disk_manifest: None,
        memory_manifest: None,
        recoverable: true,
        aux_bundles: vec![],
        events_cursor: None,
        fc_snapshot_version: None,
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

/// Finding 7 (issue #529 Testing item): `record_snapshot`'s `RETURNING
/// (xmax = 0) AS inserted` idiom against REAL Postgres — `true` on the
/// first (INSERT) landing, `false` on every idempotent re-record
/// (UPDATE via `ON CONFLICT (id) DO UPDATE`). The heartbeat reconcile
/// uses this bool to emit `SnapshotTaken` exactly once; the mocks
/// (`state.rs` MiniMeta, `api.rs`, `grpc_app.rs`) hand-roll the same
/// logic in Rust, so a misreport in the actual SQL has no other red
/// test.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn record_snapshot_returns_inserted_true_on_insert_false_on_rerecord() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    let mut row = checkpoint_row(session_id, Utc::now(), Some(1));
    let inserted = meta
        .record_snapshot(row.clone())
        .await
        .expect("first record");
    assert!(inserted, "the first landing of a snapshot id must INSERT");

    // Idempotent re-record of the SAME id (e.g. a heartbeat retry, or the
    // reconciler re-ingesting a checkpoint the eviction pipeline already
    // recorded) must UPDATE, not INSERT.
    row.last_accessed_at = Utc::now();
    let inserted_again = meta
        .record_snapshot(row.clone())
        .await
        .expect("idempotent re-record");
    assert!(
        !inserted_again,
        "a re-record of an existing snapshot id must UPDATE (xmax != 0), not INSERT again"
    );

    // A genuinely new snapshot id is, again, an INSERT.
    let other = checkpoint_row(session_id, Utc::now(), Some(2));
    let inserted_other = meta.record_snapshot(other).await.expect("second record");
    assert!(
        inserted_other,
        "a distinct snapshot id must INSERT even though a row already exists for the session"
    );
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
    // A template snapshot (session_id NULL) — exempt from checkpoint
    // retention regardless of age (the WHERE is `session_id IS NOT NULL`).
    let mut template = checkpoint_row(session_id, now - ChronoDuration::hours(1), None);
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

/// The `session_id IS NULL` reaper: a superseded base capture (referenced
/// by no `enabled_images.base_snapshot_id`) past the grace window is
/// deleted; the current (referenced) base is kept even when old; a fresh
/// orphan within grace is kept; session snapshots are never touched.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn prune_orphan_base_snapshots_reaps_superseded_only() {
    let Some(meta) = pg().await else { return };
    let now = Utc::now();

    // Three base captures (session_id NULL).
    let current_base = base_row(now - ChronoDuration::days(5));
    let orphan_old = base_row(now - ChronoDuration::days(5));
    let orphan_fresh = base_row(now - ChronoDuration::hours(1));
    for r in [&current_base, &orphan_old, &orphan_fresh] {
        meta.record_snapshot(r.clone()).await.expect("record base");
    }

    // Mark current_base as the live base of an enabled image — it must
    // survive despite its age. Unique URI so it can't ON CONFLICT a
    // sibling test's row.
    meta.upsert_enabled_image(EnabledImage {
        id: Uuid::new_v4(),
        image_uri: format!("localhost:5001/demo:base-reaper-{}", Uuid::new_v4()),
        image_config: engram_core::types::image::ImageConfig {
            name: "test".into(),
            ..Default::default()
        },
        oci_defaults: Default::default(),
        manifest_digest: "sha256:base-reaper".into(),
        disk_manifest: None,
        base_snapshot_id: Some(current_base.id),
        base_snapshot_disk_manifest: Some(ManifestRef::new()),
        base_snapshot_memory_manifest: None,
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
        soft_deleted_at: None,
    })
    .await
    .expect("enable image");

    // An old session checkpoint — the base reaper (session_id NULL only)
    // must leave it untouched; that's checkpoint_retention's job.
    let (session_id, _s) = seed_active(&meta).await;
    let sess_snap = checkpoint_row(session_id, now - ChronoDuration::days(5), None);
    meta.record_snapshot(sess_snap.clone())
        .await
        .expect("record session snap");

    let deleted = meta
        .prune_orphan_base_snapshots(ChronoDuration::hours(24))
        .await
        .expect("prune bases");

    assert!(
        deleted.contains(&orphan_old.id),
        "a superseded base past grace must be reaped",
    );
    assert!(
        !deleted.contains(&current_base.id),
        "the live base of an enabled image must never be reaped",
    );
    assert!(
        !deleted.contains(&orphan_fresh.id),
        "a fresh orphan within grace must be kept",
    );
    assert!(
        !deleted.contains(&sess_snap.id),
        "session snapshots are out of scope for the base reaper",
    );

    // Row-level effect, robust to other tests' rows in the shared DB.
    assert!(
        meta.get_snapshot(orphan_old.id)
            .await
            .expect("get")
            .is_none(),
        "reaped base row is gone",
    );
    assert!(
        meta.get_snapshot(current_base.id)
            .await
            .expect("get")
            .is_some(),
        "live base row remains",
    );
    assert!(
        meta.get_snapshot(orphan_fresh.id)
            .await
            .expect("get")
            .is_some(),
        "fresh orphan row remains",
    );
    assert!(
        meta.get_snapshot(sess_snap.id)
            .await
            .expect("get")
            .is_some(),
        "session snapshot row remains",
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
            serde_json::json!({"role": "assistant", "text": "before"}),
        )
        .await
        .expect("append e0");
    // Post-checkpoint span: a message + a PR (a surviving side-effect).
    meta.append_session_event(
        session_id,
        "agent_message",
        serde_json::json!({"role": "assistant", "text": "after"}),
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
            serde_json::json!({"role": "assistant", "text": "resumed"}),
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

/// Issue #529: `rewind_session_to_cursor` must NOT tombstone the
/// coordinator's own eviction/resume lifecycle events — `evicted`,
/// `status_changed`, `snapshot_taken`, `resumed`,
/// `recovered_from_checkpoint`. A clean evict→resume cycle appends
/// exactly this family past the cursor. ADR 0091 also excludes the clean
/// harness state markers (`harness_idle` and ADR 0089's `harness_parked`);
/// tombstoning any of them is what made
/// every resume look like a rewind even when nothing guest-derived was
/// lost.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn rewind_is_kind_scoped_to_guest_derived_events() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    let cursor = meta
        .append_session_event(
            session_id,
            "agent_message",
            serde_json::json!({"role": "assistant", "text": "before eviction"}),
        )
        .await
        .expect("append e0");

    // The exact four-event family a clean D5 evict→resume appends past
    // the cursor (idle_evictor.rs `Evicted` + `StatusChanged(->idle)` +
    // `SnapshotTaken`, then the resume's `StatusChanged(->created)`).
    for (kind, payload) in [
        ("evicted", serde_json::json!({})),
        (
            "status_changed",
            serde_json::json!({"from": "active", "to": "idle"}),
        ),
        (
            "snapshot_taken",
            serde_json::json!({"snapshot_id": Uuid::new_v4(), "size_bytes": 1}),
        ),
        (
            "status_changed",
            serde_json::json!({"from": "idle", "to": "created"}),
        ),
        ("harness_idle", serde_json::json!({})),
        ("harness_parked", serde_json::json!({})),
    ] {
        meta.append_session_event(session_id, kind, payload)
            .await
            .expect("append lifecycle event");
    }

    let lifecycle_only = meta
        .rewind_session_to_cursor(session_id, cursor)
        .await
        .expect("rewind over lifecycle-only span");
    assert_eq!(
        lifecycle_only.rolled_back, 0,
        "coordinator lifecycle events must not be tombstoned by a rewind"
    );
    assert_eq!(
        lifecycle_only.recovery_epoch, 0,
        "no-op rewind (nothing guest-derived rolled back) must not bump the epoch"
    );

    // Now interleave a genuinely guest-derived event past the same
    // cursor — that one, and only that one, must be tombstoned.
    meta.append_session_event(
        session_id,
        "agent_message",
        serde_json::json!({"role": "assistant", "text": "guest replay candidate"}),
    )
    .await
    .expect("append guest event");

    let mixed = meta
        .rewind_session_to_cursor(session_id, cursor)
        .await
        .expect("rewind over mixed span");
    assert_eq!(
        mixed.rolled_back, 1,
        "only the guest-derived event is tombstoned; lifecycle events are excluded"
    );
    assert_eq!(
        mixed.recovery_epoch, 1,
        "epoch bumps once real work rolled back"
    );

    let events = meta
        .list_session_events_since(session_id, -1, 1000)
        .await
        .expect("replay");
    let lifecycle_rewound = events
        .iter()
        .filter(|e| {
            e.idx > cursor
                && matches!(
                    e.kind.as_str(),
                    "evicted"
                        | "status_changed"
                        | "snapshot_taken"
                        | "harness_idle"
                        | "harness_parked"
                )
        })
        .any(|e| e.rewound_at.is_some());
    assert!(
        !lifecycle_rewound,
        "no lifecycle-kind event is ever tombstoned"
    );
}

/// Issue #527 Phase 1: `prompt_received` is a coordinator-authoritative
/// receipt ("the user asked at time T") that stays true across a
/// guest-state rewind — a rung-1 recovery rewinds the HARNESS's view of
/// the world, not whether the user sent the prompt. It must survive
/// `rewind_session_to_cursor` untombstoned and uncounted, or every
/// resume-with-rollback would inflate `rolled_back` by one and mask the
/// signal this issue exists to measure.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn rewind_excludes_prompt_received_from_tombstone_and_rolled_back_count() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    let cursor = meta
        .append_session_event(
            session_id,
            "agent_message",
            serde_json::json!({"role": "assistant", "text": "before"}),
        )
        .await
        .expect("append e0");

    // Post-checkpoint span: the receipt for the very prompt whose auto-
    // resume is what's being rewound (the realistic shape — the resume's
    // own lifecycle events land after the checkpoint cursor today), plus an
    // ordinary harness event that SHOULD tombstone normally.
    let receipt_idx = meta
        .append_session_event(
            session_id,
            "prompt_received",
            serde_json::json!({"prompt_id": "p-527", "at": chrono::Utc::now()}),
        )
        .await
        .expect("append prompt_received");
    meta.append_session_event(
        session_id,
        "agent_message",
        serde_json::json!({"role": "assistant", "text": "after"}),
    )
    .await
    .expect("append e1");

    let summary = meta
        .rewind_session_to_cursor(session_id, cursor)
        .await
        .expect("rewind");
    assert_eq!(
        summary.rolled_back, 1,
        "only the ordinary post-cursor event counts; prompt_received is excluded",
    );

    let events = meta
        .list_session_events_since(session_id, -1, 1000)
        .await
        .expect("replay");
    let receipt_row = events
        .iter()
        .find(|e| e.idx == receipt_idx)
        .expect("receipt row present");
    assert_eq!(receipt_row.kind, "prompt_received");
    assert!(
        receipt_row.rewound_at.is_none(),
        "prompt_received must survive the rewind untombstoned",
    );

    let other_rolled: Vec<_> = events
        .iter()
        .filter(|e| e.idx > cursor && e.idx != receipt_idx && e.rewound_at.is_some())
        .collect();
    assert_eq!(
        other_rolled.len(),
        1,
        "the ordinary harness event is tombstoned as usual",
    );
}

/// Issue #527 Phase 1: `prompt_received_seconds_ago` resolves the receipt
/// row's age by `(session_id, prompt_id)` — the join key
/// `engram_prompt_to_run_started_seconds` uses — and correctly returns
/// `None` for an unknown / never-received prompt_id (the env-seeded
/// initial prompt's case), never an error.
///
/// PR #556 review finding #1: the elapsed seconds are computed PG-side
/// (`NOW() - created_at`) rather than handed back as a raw `created_at`
/// timestamp for the caller to diff against its own process clock — this
/// test asserts the *elapsed* value directly, which is what makes the
/// live-Postgres assertion below immune to coordinator/test-process clock
/// skew in the first place.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn prompt_received_seconds_ago_resolves_by_prompt_id_and_misses_cleanly() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    assert!(
        meta.prompt_received_seconds_ago(session_id, "never-sent")
            .await
            .expect("lookup miss")
            .is_none(),
        "an unknown prompt_id must resolve to None, not an error",
    );

    let before = Utc::now();
    meta.append_session_event(
        session_id,
        "prompt_received",
        serde_json::json!({"prompt_id": "p-resolve", "at": before}),
    )
    .await
    .expect("append receipt");

    let secs_ago = meta
        .prompt_received_seconds_ago(session_id, "p-resolve")
        .await
        .expect("lookup hit")
        .expect("receipt present");
    assert!(
        (0.0..5.0).contains(&secs_ago),
        "receipt was just inserted, so its age must be small and non-negative, \
         got {secs_ago}",
    );

    // A different prompt_id on the same session is a clean miss, not a
    // false-positive match against the sibling receipt.
    assert!(meta
        .prompt_received_seconds_ago(session_id, "p-other")
        .await
        .expect("lookup miss 2")
        .is_none(),);
}

/// ADR 0056 Phase 2: a session's profile-granted capabilities round-trip
/// through `session_capabilities` — covering the empty no-op, idempotent
/// re-bind (ON CONFLICT DO NOTHING), and the `resource` '' <-> Option::None
/// mapping. This is the data-plumbing the broker reads to clamp (no
/// enforcement yet).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn session_capabilities_bind_get_round_trip() {
    let Some(meta) = pg().await else {
        return;
    };
    let (session_id, _sandbox) = seed_active(&meta).await;

    // Empty bind is a no-op; get returns nothing.
    meta.bind_session_capabilities(session_id, &[])
        .await
        .expect("empty bind");
    assert!(meta
        .get_session_capabilities(session_id)
        .await
        .expect("get empty")
        .is_empty());

    let caps = vec![
        Capability::parse("github:contents:write@cortexapps/engrams").unwrap(),
        Capability::parse("datadog:logs:read").unwrap(),
    ];
    meta.bind_session_capabilities(session_id, &caps)
        .await
        .expect("bind");
    // Idempotent: re-binding the same set must not error or duplicate.
    meta.bind_session_capabilities(session_id, &caps)
        .await
        .expect("idempotent re-bind");

    let got = meta
        .get_session_capabilities(session_id)
        .await
        .expect("get");
    assert_eq!(got.len(), 2, "two distinct capabilities, no duplicates");
    assert!(
        got.contains(&Capability::parse("github:contents:write@cortexapps/engrams").unwrap()),
        "resource-scoped capability round-trips verbatim",
    );
    let dd = got
        .iter()
        .find(|c| c.provider == "datadog")
        .expect("datadog cap present");
    assert_eq!(
        dd.resource, None,
        "the '' resource sentinel maps back to Option::None",
    );
}

/// ADR 0056 Phase 3b-2: the compiled integration policy round-trips through
/// `session_integration_policy` (upsert + JSON verbatim), so a queued
/// re-prepare / resume can re-resolve its inject refs without the orchestrator.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn session_integration_policy_round_trip() {
    let Some(meta) = pg().await else {
        return;
    };
    let (session_id, _sandbox) = seed_active(&meta).await;

    // Absent → None.
    assert!(meta
        .get_session_integration_policy(session_id)
        .await
        .expect("get empty")
        .is_none());

    let json = r#"{"injects":[{"hosts":["api.datadoghq.com"],"header_name":"DD-API-KEY","header_template":"{}","secret_ref":"datadog-api-key","methods":["GET"],"path_globs":["/api/v2/logs*"]}]}"#;
    meta.bind_session_integration_policy(session_id, json)
        .await
        .expect("bind");
    // Upsert: a re-bind replaces, no error.
    meta.bind_session_integration_policy(session_id, json)
        .await
        .expect("re-bind");

    let got = meta
        .get_session_integration_policy(session_id)
        .await
        .expect("get")
        .expect("policy present");
    // Parses back to the typed policy the coordinator resolves.
    let policy = engram_core::types::IntegrationPolicy::parse(&got)
        .expect("valid json")
        .expect("non-empty");
    assert_eq!(policy.injects.len(), 1);
    assert_eq!(policy.injects[0].secret_ref, "datadog-api-key");
    assert_eq!(policy.injects[0].methods, vec!["GET".to_string()]);
}

/// ADR 0077 phase 1: `record_snapshot` advances the session's durable
/// head in the SAME transaction, monotonically by created_at — a newer
/// commit advances it, an older/out-of-order re-record never regresses
/// it, and a base capture (session_id IS NULL) never touches it.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn record_snapshot_advances_durable_head_monotonically() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    assert!(
        meta.durable_head_snapshot(session_id)
            .await
            .unwrap()
            .is_none(),
        "fresh session has no durable head",
    );

    let t0 = Utc::now();
    let older = checkpoint_row(session_id, t0, Some(10));
    let newer = checkpoint_row(session_id, t0 + ChronoDuration::seconds(30), Some(20));

    meta.record_snapshot(older.clone()).await.unwrap();
    assert_eq!(
        meta.durable_head_snapshot(session_id).await.unwrap(),
        Some(older.id),
        "first commit becomes the head",
    );

    meta.record_snapshot(newer.clone()).await.unwrap();
    assert_eq!(
        meta.durable_head_snapshot(session_id).await.unwrap(),
        Some(newer.id),
        "a newer commit advances the head",
    );

    // Out-of-order re-record of the OLDER row must NOT regress the head.
    meta.record_snapshot(older.clone()).await.unwrap();
    assert_eq!(
        meta.durable_head_snapshot(session_id).await.unwrap(),
        Some(newer.id),
        "an older re-record must not regress the durable head",
    );

    // A base capture never touches a session head (no session_id).
    meta.record_snapshot(base_row(t0 + ChronoDuration::seconds(60)))
        .await
        .unwrap();
    assert_eq!(
        meta.durable_head_snapshot(session_id).await.unwrap(),
        Some(newer.id),
        "a base capture must not touch any session's durable head",
    );
}

/// ADR 0077 phase 3: the RuntimeSpec round-trips, and a session with no
/// spec row reads `None` (the pre-0071 fallback that boots with base
/// skills — the queued-skills TODO(P1-D) fix is opt-in on the write).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn runtime_spec_round_trips_and_absent_reads_none() {
    let Some(meta) = pg().await else { return };
    let (session_id, _sandbox) = seed_active(&meta).await;

    assert!(
        meta.get_session_runtime_spec(session_id)
            .await
            .unwrap()
            .is_none(),
        "a session with no spec row reads None",
    );

    let spec = engram_core::types::runtime_spec::RuntimeSpec::new(
        vec!["git".into(), "browser".into()],
        Some("claude".into()),
        Some("/workspace".into()),
    );
    meta.put_session_runtime_spec(session_id, &spec)
        .await
        .unwrap();
    assert_eq!(
        meta.get_session_runtime_spec(session_id).await.unwrap(),
        Some(spec.clone()),
        "the persisted spec round-trips (the queued/resume boot re-resolves these skills)",
    );

    // Upsert replaces.
    let spec2 = engram_core::types::runtime_spec::RuntimeSpec::new(vec!["git".into()], None, None);
    meta.put_session_runtime_spec(session_id, &spec2)
        .await
        .unwrap();
    assert_eq!(
        meta.get_session_runtime_spec(session_id).await.unwrap(),
        Some(spec2),
    );
}
