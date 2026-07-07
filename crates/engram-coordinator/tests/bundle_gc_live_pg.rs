//! Live-Postgres tests for ADR 0035's bundle-generation GC: the
//! `snapshots.aux_bundles` pin set, the `bundle_gc_candidates`
//! upsert/list/delete cycle, and a full `run_one_bundle_sweep` against
//! a `LocalBlobStorage` — pinning the property the 2026-06-03 incident
//! demanded: **a generation referenced by any snapshot row is never
//! deleted; an unreferenced one is, after the grace period.**
//!
//! `#[ignore]`'d by default; requires Postgres reachable at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test bundle_gc_live_pg -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_coordinator::bundle_gc::run_one_bundle_sweep;
use engram_coordinator::chunk_gc::{ChunkGcConfig, SweepMode};
use engram_core::traits::{BlobStorage, MetadataStore};
use engram_core::types::sandbox::{AuxBundleRef, AuxRoDrive};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::SnapshotId;
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

/// Unique-per-run fake sha256 hex so concurrent CI runs don't trample.
fn fake_sha() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

async fn seed_snapshot_with_bundles(
    meta: &Arc<dyn MetadataStore>,
    aux_bundles: Vec<AuxBundleRef>,
) -> SnapshotId {
    let id = SnapshotId::new();
    meta.record_snapshot(SnapshotRecord {
        id,
        session_id: None,
        host_id: None,
        image_version: "bundle-gc-fixture".into(),
        size_bytes: 0,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: None,
        memory_manifest: None,
        recoverable: true,
        aux_bundles,
        events_cursor: None,
        fc_snapshot_version: None,
    })
    .await
    .expect("seed snapshot");
    id
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn aux_bundles_round_trip_and_pin_set() {
    let Some(meta) = connect().await else {
        return;
    };
    let sha = fake_sha();
    let refs = vec![AuxBundleRef {
        drive_id: "skills".into(),
        sha256: sha.clone(),
    }];
    let id = seed_snapshot_with_bundles(&meta, refs.clone()).await;

    // Round-trip through the row.
    let got = meta.get_snapshot(id).await.expect("get").expect("present");
    assert_eq!(got.aux_bundles, refs);

    // Pin set surfaces it.
    let pins = meta.bundle_pin_set().await.expect("pin set");
    assert!(
        pins.iter().any(|r| r.sha256 == sha),
        "pin set must contain the seeded generation"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn bundle_gc_candidate_upsert_list_delete_round_trip() {
    let Some(meta) = connect().await else {
        return;
    };
    let sha = fake_sha();
    meta.upsert_bundle_gc_candidate(&sha).await.expect("upsert");
    // Sticky first_seen_at: re-upsert must not push it past a
    // future cutoff.
    meta.upsert_bundle_gc_candidate(&sha)
        .await
        .expect("re-upsert");
    let future = Utc::now() + chrono::Duration::hours(1);
    let expired = meta
        .list_expired_bundle_gc_candidates(future, 10_000)
        .await
        .expect("list");
    assert!(expired.contains(&sha));
    // Not expired against a past cutoff.
    let past = Utc::now() - chrono::Duration::hours(1);
    let not_expired = meta
        .list_expired_bundle_gc_candidates(past, 10_000)
        .await
        .expect("list past");
    assert!(!not_expired.contains(&sha));
    meta.delete_bundle_gc_candidates(std::slice::from_ref(&sha))
        .await
        .expect("delete");
    let after = meta
        .list_expired_bundle_gc_candidates(future, 10_000)
        .await
        .expect("list after");
    assert!(!after.contains(&sha));
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn sweep_deletes_unpinned_after_grace_and_never_touches_pinned() {
    let Some(meta) = connect().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let blob: Arc<dyn BlobStorage> =
        Arc::new(engram_storage_local::LocalBlobStorage::new(tmp.path()));

    // One pinned generation (a snapshot references it), one garbage.
    let pinned_sha = fake_sha();
    let garbage_sha = fake_sha();
    seed_snapshot_with_bundles(
        &meta,
        vec![AuxBundleRef {
            drive_id: "skills".into(),
            sha256: pinned_sha.clone(),
        }],
    )
    .await;
    for sha in [&pinned_sha, &garbage_sha] {
        blob.put(
            &AuxRoDrive::blob_key(sha),
            bytes::Bytes::from_static(b"squashfs-bytes"),
        )
        .await
        .expect("publish");
    }

    // Zero grace so the promote pass fires immediately on the second
    // sweep (first sweep marks the candidate, promote sees it expired).
    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        ..ChunkGcConfig::default()
    };
    let report1 = run_one_bundle_sweep(meta.clone(), blob.clone(), &cfg, SweepMode::Full)
        .await
        .expect("sweep 1");
    assert!(report1.candidates_marked >= 1);
    // With zero grace the candidate can promote within the same sweep
    // (first_seen_at < now by the time the promote pass runs); run a
    // second sweep to cover the row either way.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let report2 = run_one_bundle_sweep(meta.clone(), blob.clone(), &cfg, SweepMode::Full)
        .await
        .expect("sweep 2");
    assert!(
        report1.promoted_deletes + report2.promoted_deletes >= 1,
        "garbage generation should promote within two zero-grace sweeps: {report1:?} {report2:?}"
    );

    // The property under test: pinned survives, garbage is gone.
    assert!(
        blob.exists(&AuxRoDrive::blob_key(&pinned_sha))
            .await
            .unwrap(),
        "pinned generation must never be deleted"
    );
    assert!(
        !blob
            .exists(&AuxRoDrive::blob_key(&garbage_sha))
            .await
            .unwrap(),
        "unpinned generation must be deleted after grace"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn dry_run_marks_nothing() {
    let Some(meta) = connect().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let blob: Arc<dyn BlobStorage> =
        Arc::new(engram_storage_local::LocalBlobStorage::new(tmp.path()));
    let garbage_sha = fake_sha();
    blob.put(
        &AuxRoDrive::blob_key(&garbage_sha),
        bytes::Bytes::from_static(b"x"),
    )
    .await
    .unwrap();
    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        ..ChunkGcConfig::default()
    };
    let report = run_one_bundle_sweep(meta.clone(), blob.clone(), &cfg, SweepMode::DryRun)
        .await
        .expect("dry run");
    assert!(report.candidates_marked >= 1);
    // Dry run neither parked a candidate nor deleted the blob.
    assert!(blob
        .exists(&AuxRoDrive::blob_key(&garbage_sha))
        .await
        .unwrap());
    let future = Utc::now() + chrono::Duration::hours(1);
    let expired = meta
        .list_expired_bundle_gc_candidates(future, 10_000)
        .await
        .expect("list");
    assert!(!expired.contains(&garbage_sha));
}
