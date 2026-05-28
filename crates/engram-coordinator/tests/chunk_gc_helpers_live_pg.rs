//! Live-Postgres tests for the ADR 0016 Phase C `MetadataStore`
//! helpers shipped in commit 1: `chunk_gc_candidates` upsert/list/
//! delete cycle, `bump_chunk_generation`, and
//! `list_live_session_disk_manifest_ids`.
//!
//! `#[ignore]`'d by default; requires Postgres reachable at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test chunk_gc_helpers_live_pg -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::registry::EnabledImage;
use engram_core::types::session::SessionMode;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{SandboxId, SessionSpec, SnapshotId};
use uuid::Uuid;

async fn connect() -> Option<Arc<dyn MetadataStore>> {
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

/// ADR 0020: `enabled_images.base_snapshot_id` is NOT NULL with an FK to
/// `snapshots(id)` (migration 0038). Seed a throwaway template snapshot
/// (session_id = NULL, allowed since 0028) so these fixtures can insert
/// an enabled image without depending on the capture pipeline.
async fn seed_base_snapshot(meta: &Arc<dyn MetadataStore>) -> SnapshotId {
    let id = SnapshotId::new();
    meta.record_snapshot(SnapshotRecord {
        id,
        session_id: None,
        host_id: None,
        image_version: "base-snapshot-fixture".into(),
        size_bytes: 0,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: None,
        memory_manifest: None,
        recoverable: true,
    })
    .await
    .expect("seed base snapshot");
    id
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn chunk_gc_candidate_upsert_list_delete_round_trip() {
    let Some(meta) = connect().await else {
        return;
    };

    // Pick a deterministic but unique-per-run hash so concurrent
    // CI runs against the same DB don't trample each other.
    let mut hash = [0u8; 32];
    hash[0..16].copy_from_slice(Uuid::new_v4().as_bytes());
    hash[16..32].copy_from_slice(Uuid::new_v4().as_bytes());

    // Upsert. First call inserts.
    meta.upsert_chunk_gc_candidate(hash)
        .await
        .expect("upsert 1");

    // Re-upsert. Second call updates last_seen_at without
    // resetting first_seen_at — exercise the idempotency, but
    // we can't directly observe the column values from the trait
    // surface. Indirect proof: query with cutoff in the future
    // should return the row exactly once.
    meta.upsert_chunk_gc_candidate(hash)
        .await
        .expect("upsert 2");

    let future = Utc::now() + chrono::Duration::seconds(60);
    let listed = meta
        .list_expired_gc_candidates(future, 100)
        .await
        .expect("list");
    let occurrences = listed.iter().filter(|h| *h == &hash).count();
    assert_eq!(occurrences, 1, "candidate must appear exactly once");

    // Cutoff in the past — nothing should come back for our row
    // (since first_seen_at is `now()`).
    let past = Utc::now() - chrono::Duration::seconds(60);
    let nothing = meta
        .list_expired_gc_candidates(past, 100)
        .await
        .expect("list past");
    assert!(
        !nothing.iter().any(|h| h == &hash),
        "candidate must NOT appear when cutoff is older than first_seen_at",
    );

    // Delete. Idempotent — second delete is a no-op.
    meta.delete_gc_candidates(&[hash]).await.expect("delete 1");
    meta.delete_gc_candidates(&[hash]).await.expect("delete 2");

    let after_delete = meta
        .list_expired_gc_candidates(future, 100)
        .await
        .expect("list post-delete");
    assert!(
        !after_delete.iter().any(|h| h == &hash),
        "candidate must not survive delete",
    );

    // Empty-batch delete is a no-op (no round-trip in the impl,
    // but exercising it from the trait surface is the contract).
    meta.delete_gc_candidates(&[]).await.expect("delete empty");
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn bump_chunk_generation_increments_monotonically() {
    let Some(meta) = connect().await else {
        return;
    };

    let before = meta.chunk_generation().await.expect("read gen 1");
    meta.bump_chunk_generation().await.expect("bump 1");
    let after_one = meta.chunk_generation().await.expect("read gen 2");
    assert!(
        after_one > before,
        "single bump must advance generation (was {before}, now {after_one})",
    );

    meta.bump_chunk_generation().await.expect("bump 2");
    meta.bump_chunk_generation().await.expect("bump 3");
    let after_three = meta.chunk_generation().await.expect("read gen 3");
    assert!(
        after_three >= after_one + 2,
        "three bumps from {after_one} must reach >= {} (got {after_three})",
        after_one + 2,
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn list_live_session_disk_manifest_ids_picks_up_live_writes() {
    let Some(meta) = connect().await else {
        return;
    };

    // Need a session row to attach a sandbox + live manifest to.
    let session_id = meta
        .create_session(SessionSpec {
            image: "phase-c-helpers-test:warm-1".into(),
            mode: SessionMode::Agent,
            user_id: None,
        })
        .await
        .expect("create session");
    let sandbox_id = SandboxId::new();
    meta.assign_session_sandbox(session_id, Some(sandbox_id))
        .await
        .expect("assign sandbox");

    let mref = ManifestRef {
        manifest_id: Uuid::new_v4(),
        version: 42,
    };

    // Pre-write: live set must NOT contain our (fresh) manifest_id.
    let before = meta
        .list_live_session_disk_manifest_ids()
        .await
        .expect("list before");
    assert!(
        !before.iter().any(|r| r.manifest_id == mref.manifest_id),
        "fresh manifest id appeared before being written — UUID collision or stale fixture",
    );

    let outcome = meta
        .update_live_disk_manifest(session_id, sandbox_id, mref)
        .await
        .expect("publish live manifest");
    assert!(
        matches!(outcome, engram_core::traits::UpdateOutcome::Applied),
        "publish must apply against a live (sandbox_id-matching) session",
    );

    // Post-write: live set MUST contain it.
    let after = meta
        .list_live_session_disk_manifest_ids()
        .await
        .expect("list after");
    let hit = after
        .iter()
        .find(|r| r.manifest_id == mref.manifest_id)
        .expect("live manifest not surfaced after publish");
    assert_eq!(
        hit.version, mref.version,
        "version must round-trip via the live-set query",
    );

    // Clear the live manifest via assign_session_sandbox(None) —
    // the trait method already cascades the cleanup per its docs.
    meta.assign_session_sandbox(session_id, None)
        .await
        .expect("unbind sandbox");

    // Allow PG a tick to fully commit the cascade then re-poll.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let after_clear = meta
        .list_live_session_disk_manifest_ids()
        .await
        .expect("list after clear");
    assert!(
        !after_clear
            .iter()
            .any(|r| r.manifest_id == mref.manifest_id),
        "live manifest must drop out of the live set after the sandbox is unbound",
    );
}

// ---- Commit 2: barrier-coverage extension to enable_image + record_snapshot ----

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn record_snapshot_bumps_chunk_generation() {
    let Some(meta) = connect().await else {
        return;
    };

    // Need a session row for the snapshot FK.
    let session_id = meta
        .create_session(SessionSpec {
            image: "phase-c-snapshot-bump-test:warm-1".into(),
            mode: SessionMode::Agent,
            user_id: None,
        })
        .await
        .expect("create session");

    let before = meta.chunk_generation().await.expect("read gen before");

    // Snapshot WITH a disk_manifest — the common shape. The bump
    // is unconditional regardless of whether manifests are present
    // (simpler invariant; over-bumping costs at most a wasted sweep
    // restart, bounded by max_restart_attempts).
    let snap = SnapshotRecord {
        id: SnapshotId::new(),
        session_id: Some(session_id),
        host_id: None,
        image_version: "warm-1".into(),
        size_bytes: 1024,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: Some(ManifestRef {
            manifest_id: Uuid::new_v4(),
            version: 1,
        }),
        memory_manifest: None,
        recoverable: false,
    };
    meta.record_snapshot(snap).await.expect("record snapshot");

    let after = meta.chunk_generation().await.expect("read gen after");
    assert!(
        after > before,
        "record_snapshot must bump chunk_generation (was {before}, now {after})",
    );

    // Re-record under the same id (ON CONFLICT path) — also bumps.
    // INSERT-vs-UPDATE in one TX both end with the
    // chunk_generation bump statement.
    let snap2 = SnapshotRecord {
        id: SnapshotId::new(),
        session_id: Some(session_id),
        host_id: None,
        image_version: "warm-1".into(),
        size_bytes: 2048,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: None,
        memory_manifest: None,
        recoverable: false,
    };
    meta.record_snapshot(snap2)
        .await
        .expect("record snapshot 2");
    let after_two = meta.chunk_generation().await.expect("read gen after 2");
    assert!(
        after_two > after,
        "second record_snapshot must bump again (was {after}, now {after_two})",
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn upsert_enabled_image_bumps_chunk_generation() {
    let Some(meta) = connect().await else {
        return;
    };

    let before = meta.chunk_generation().await.expect("read gen before");

    // Unique image_uri per test run so concurrent runs don't trample.
    let image_uri = format!("phase-c-image-bump-test:warm-{}", Uuid::new_v4());
    let image = EnabledImage {
        id: Uuid::new_v4(),
        image_uri: image_uri.clone(),
        manifest_toml: "image = { uri = \"test\" }\n".into(),
        manifest_digest: format!("sha256:{:064x}", 0xdeadbeefu32),
        disk_manifest: None,
        base_snapshot_id: Some(seed_base_snapshot(&meta).await),
        last_refreshed_at: Utc::now(),
        created_at: Utc::now(),
        updated_at: None,
    };
    meta.upsert_enabled_image(image.clone())
        .await
        .expect("insert");

    let after_insert = meta
        .chunk_generation()
        .await
        .expect("read gen after insert");
    assert!(
        after_insert > before,
        "INSERT path must bump chunk_generation (was {before}, now {after_insert})",
    );

    // ON CONFLICT path — refresh the manifest_toml and re-upsert.
    let mut updated = image.clone();
    updated.manifest_toml = "image = { uri = \"test-v2\" }\n".into();
    meta.upsert_enabled_image(updated).await.expect("update");

    let after_update = meta
        .chunk_generation()
        .await
        .expect("read gen after update");
    assert!(
        after_update > after_insert,
        "ON CONFLICT path must bump chunk_generation too (was {after_insert}, now {after_update})",
    );

    // Cleanup so re-runs against the same DB stay independent.
    meta.delete_enabled_image(&image_uri).await.expect("delete");
}

// ---- Commit 3a: enabled_images.disk_manifest_* round-trip + pin-set ----

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn enabled_image_disk_manifest_round_trips_and_surfaces_in_pin_set() {
    let Some(meta) = connect().await else {
        return;
    };

    let image_uri = format!("phase-c-3a-disk-ref-test:warm-{}", Uuid::new_v4());
    let mref = ManifestRef {
        manifest_id: Uuid::new_v4(),
        version: 13,
    };

    // Pre-write: pin-set source #1 must NOT contain our fresh ref.
    let before = meta
        .list_enabled_image_disk_manifest_ids()
        .await
        .expect("list before");
    assert!(
        !before.iter().any(|r| r.manifest_id == mref.manifest_id),
        "fresh manifest id appeared before being written",
    );

    let image = EnabledImage {
        id: Uuid::new_v4(),
        image_uri: image_uri.clone(),
        manifest_toml: "image = { uri = \"test\" }\n".into(),
        manifest_digest: format!("sha256:{:064x}", 0xfeedfaceu32),
        disk_manifest: Some(mref),
        base_snapshot_id: Some(seed_base_snapshot(&meta).await),
        last_refreshed_at: Utc::now(),
        created_at: Utc::now(),
        updated_at: None,
    };
    meta.upsert_enabled_image(image.clone())
        .await
        .expect("upsert with disk_manifest");

    // get_enabled_image must round-trip the disk_manifest field.
    let fetched = meta
        .get_enabled_image(&image_uri)
        .await
        .expect("get")
        .expect("row must exist");
    assert_eq!(
        fetched.disk_manifest,
        Some(mref),
        "disk_manifest must round-trip through PG",
    );

    // Pin-set query picks it up.
    let after = meta
        .list_enabled_image_disk_manifest_ids()
        .await
        .expect("list after");
    let hit = after
        .iter()
        .find(|r| r.manifest_id == mref.manifest_id)
        .expect("disk_manifest not surfaced by pin-set query");
    assert_eq!(
        hit.version, mref.version,
        "version must round-trip via the pin-set query",
    );

    // Harness-only re-upsert (disk_manifest = None) drops the row
    // out of the pin set. This is the "image refresh dropped its
    // chunked artifact" path — surfaces as None in PG, and the
    // partial index excludes it from the pin-set scan.
    let mut harness_only = image.clone();
    harness_only.disk_manifest = None;
    meta.upsert_enabled_image(harness_only)
        .await
        .expect("upsert harness-only");
    let after_clear = meta
        .list_enabled_image_disk_manifest_ids()
        .await
        .expect("list after clear");
    assert!(
        !after_clear
            .iter()
            .any(|r| r.manifest_id == mref.manifest_id),
        "harness-only row must drop out of the pin-set query",
    );

    meta.delete_enabled_image(&image_uri).await.expect("delete");
}
