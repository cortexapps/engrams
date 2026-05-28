//! Live-Postgres integration test for ADR 0007 / Phase 6 additive:
//! `SnapshotRecord.disk_manifest` round-trips through the
//! `disk_manifest_id` + `disk_manifest_version` columns added by
//! migration 0018.
//!
//! `#[ignore]`'d by default; requires Postgres reachable at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test snapshot_disk_manifest_persistence -- --ignored
//! ```

use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::{SessionSpec, SnapshotRecord};
use engram_core::SnapshotId;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn snapshot_disk_manifest_round_trips_through_pg() {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skipping: ENGRAM_TEST_DATABASE_URL not set. Run with `just db-up` first; \
                 default URL is postgres://engram:engram@localhost:5435/engram",
            );
            return;
        }
    };

    let store = engram_postgres::PostgresStore::connect(&database_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    let meta: Arc<dyn MetadataStore> = Arc::new(store);

    // Need a parent session row (FK on snapshots.session_id).
    let session_id = meta
        .create_session(SessionSpec {
            image: "snap-disk-manifest-test:warm-1".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            user_id: None,
        })
        .await
        .expect("create session");

    // Case 1: snapshot WITH a disk_manifest — the chunked write path
    // produces this shape (VZ today, FC after Phase 4).
    let mref = ManifestRef {
        manifest_id: Uuid::new_v4(),
        version: 7,
    };
    let snap_with = SnapshotRecord {
        id: SnapshotId::new(),
        session_id: Some(session_id),
        host_id: None,
        image_version: "warm-1".into(),
        size_bytes: 1024,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: Some(mref),
        memory_manifest: None,
        recoverable: false,
    };
    meta.record_snapshot(snap_with.clone())
        .await
        .expect("record snapshot with disk_manifest");

    // Case 2: snapshot WITHOUT a disk_manifest — legacy / backends
    // that haven't wired chunked snapshot.
    let snap_without = SnapshotRecord {
        id: SnapshotId::new(),
        ..snap_with.clone()
    };
    let mut snap_without = snap_without;
    snap_without.disk_manifest = None;
    meta.record_snapshot(snap_without.clone())
        .await
        .expect("record snapshot without disk_manifest");

    // Read back via `list_snapshots_for_session` (returns newest first).
    let listed = meta
        .list_snapshots_for_session(session_id)
        .await
        .expect("list snapshots");
    assert!(
        listed.len() >= 2,
        "expected at least two snapshots; got {}",
        listed.len(),
    );

    let recovered_with = listed
        .iter()
        .find(|r| r.id == snap_with.id)
        .expect("seeded WITH-manifest row not returned by list");
    assert_eq!(
        recovered_with.disk_manifest,
        Some(mref),
        "disk_manifest must round-trip via the new columns",
    );

    let recovered_without = listed
        .iter()
        .find(|r| r.id == snap_without.id)
        .expect("seeded WITHOUT-manifest row not returned by list");
    assert_eq!(
        recovered_without.disk_manifest, None,
        "missing disk_manifest must read back as None",
    );
}
