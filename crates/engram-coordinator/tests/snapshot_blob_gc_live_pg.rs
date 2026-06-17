//! Live-Postgres tests for the ADR 0028 addendum snapshot-blob GC: the
//! `snapshots`-row pin set, the `snapshot_blob_gc_candidates`
//! upsert/list/delete cycle, and a full `run_one_snapshot_blob_sweep`
//! against a `LocalBlobStorage`. Pins the properties that make the
//! recurring "recoverable=true but blobs gone" brick impossible:
//!
//! - a `snapshots/<id>/` blob whose row exists is NEVER deleted — even
//!   for a `session_id IS NULL` template/base snapshot (the red-team
//!   ship-blocker: an incomplete pin set would brick fleet-wide
//!   cold-create);
//! - an orphan blob with no row IS deleted, after the grace period;
//! - a candidate that becomes re-pinned (the upload-before-record
//!   window every capture has) is skipped, not deleted.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! just db-up
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test snapshot_blob_gc_live_pg -- --ignored
//! ```

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_chunk_store::snapshot_blob::{sidecar_blob_key, state_blob_key};
use engram_coordinator::chunk_gc::{ChunkGcConfig, SweepMode};
use engram_coordinator::snapshot_blob_gc::run_one_snapshot_blob_sweep;
use engram_core::traits::{BlobStorage, MetadataStore};
use engram_core::types::session::{SessionMode, SessionSpec};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{SessionId, SnapshotId};

async fn connect() -> Option<Arc<dyn MetadataStore>> {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping: ENGRAM_TEST_DATABASE_URL not set (run `just db-up`)");
            return None;
        }
    };
    let store = engram_postgres::PostgresStore::connect(&database_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    Some(Arc::new(store))
}

/// Create a real session row so a session-bound snapshot satisfies the
/// `snapshots_session_id_fkey` FK.
async fn seed_session(meta: &Arc<dyn MetadataStore>) -> SessionId {
    meta.create_session(SessionSpec {
        image: "localhost:5001/snapshot-blob-gc:test".into(),
        mode: SessionMode::Agent,
    })
    .await
    .expect("create session")
}

/// Record a snapshot row. `session_id = None` is the template/base
/// shape (`build_base_snapshot`); `Some` is a session capture (the FK
/// requires a real session — see `seed_session`). The pin set is
/// `SELECT id FROM snapshots`, so both must pin their blobs.
async fn seed_snapshot_row(
    meta: &Arc<dyn MetadataStore>,
    session_id: Option<SessionId>,
) -> SnapshotId {
    let id = SnapshotId::new();
    meta.record_snapshot(SnapshotRecord {
        id,
        session_id,
        host_id: None,
        image_version: "snapshot-blob-gc-fixture".into(),
        size_bytes: 0,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: None,
        memory_manifest: None,
        recoverable: true,
        aux_bundles: Vec::new(),
        events_cursor: None,
    })
    .await
    .expect("seed snapshot row");
    id
}

async fn put_portable_blobs(blob: &Arc<dyn BlobStorage>, id: SnapshotId) {
    for key in [state_blob_key(id), sidecar_blob_key(id)] {
        blob.put(&key, bytes::Bytes::from_static(b"fc-state-or-sidecar"))
            .await
            .expect("put portable blob");
    }
}

fn zero_grace() -> ChunkGcConfig {
    ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        ..ChunkGcConfig::default()
    }
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn pin_set_includes_session_and_template_rows() {
    let Some(meta) = connect().await else {
        return;
    };
    let sess = seed_session(&meta).await;
    let session_snap = seed_snapshot_row(&meta, Some(sess)).await;
    let template_snap = seed_snapshot_row(&meta, None).await;
    let pins: HashSet<SnapshotId> = meta
        .snapshot_blob_pin_set()
        .await
        .expect("pin set")
        .into_iter()
        .collect();
    assert!(pins.contains(&session_snap), "session snapshot must pin");
    assert!(
        pins.contains(&template_snap),
        "session_id NULL template snapshot must pin (else cold-create bricks)"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn candidate_upsert_list_delete_round_trip() {
    let Some(meta) = connect().await else {
        return;
    };
    let id = SnapshotId::new();
    meta.upsert_snapshot_blob_gc_candidate(id)
        .await
        .expect("upsert");
    meta.upsert_snapshot_blob_gc_candidate(id)
        .await
        .expect("re-upsert (sticky first_seen)");
    let future = Utc::now() + chrono::Duration::hours(1);
    assert!(meta
        .list_expired_snapshot_blob_gc_candidates(future, 10_000)
        .await
        .expect("list")
        .contains(&id));
    let past = Utc::now() - chrono::Duration::hours(1);
    assert!(!meta
        .list_expired_snapshot_blob_gc_candidates(past, 10_000)
        .await
        .expect("list past")
        .contains(&id));
    meta.delete_snapshot_blob_gc_candidates(std::slice::from_ref(&id))
        .await
        .expect("delete");
    assert!(!meta
        .list_expired_snapshot_blob_gc_candidates(future, 10_000)
        .await
        .expect("list after")
        .contains(&id));
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn sweep_deletes_orphan_keeps_pinned_including_template() {
    let Some(meta) = connect().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let blob: Arc<dyn BlobStorage> =
        Arc::new(engram_storage_local::LocalBlobStorage::new(tmp.path()));

    // Pinned session snapshot, pinned `session_id NULL` template
    // snapshot (the ship-blocker case), and an orphan blob set with no
    // row.
    let sess = seed_session(&meta).await;
    let pinned_session = seed_snapshot_row(&meta, Some(sess)).await;
    let pinned_template = seed_snapshot_row(&meta, None).await;
    let orphan = SnapshotId::new();
    for id in [pinned_session, pinned_template, orphan] {
        put_portable_blobs(&blob, id).await;
    }

    let cfg = zero_grace();
    let r1 = run_one_snapshot_blob_sweep(meta.clone(), blob.clone(), &cfg, SweepMode::Full)
        .await
        .expect("sweep 1");
    assert!(r1.candidates_marked >= 1, "orphan must be marked: {r1:?}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r2 = run_one_snapshot_blob_sweep(meta.clone(), blob.clone(), &cfg, SweepMode::Full)
        .await
        .expect("sweep 2");
    assert!(
        r1.promoted_deletes + r2.promoted_deletes >= 1,
        "orphan must promote within two zero-grace sweeps: {r1:?} {r2:?}"
    );

    // The property: every pinned id's blobs survive; the orphan's are gone.
    for id in [pinned_session, pinned_template] {
        assert!(
            blob.exists(&state_blob_key(id)).await.unwrap(),
            "pinned snapshot {id} state.bin must survive"
        );
        assert!(blob.exists(&sidecar_blob_key(id)).await.unwrap());
    }
    assert!(
        !blob.exists(&state_blob_key(orphan)).await.unwrap(),
        "orphan state.bin must be deleted after grace"
    );
    assert!(!blob.exists(&sidecar_blob_key(orphan)).await.unwrap());
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn promote_skips_candidate_that_became_repinned() {
    // The upload-before-record window every capture has: a sweep marked
    // the id a candidate while its blob was uploaded but its row not yet
    // recorded; the row then landed. The promote pass must re-verify the
    // pin set and NOT delete it.
    let Some(meta) = connect().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let blob: Arc<dyn BlobStorage> =
        Arc::new(engram_storage_local::LocalBlobStorage::new(tmp.path()));

    let id = SnapshotId::new();
    put_portable_blobs(&blob, id).await;
    // Prior sweep marked it (blob present, no row yet).
    meta.upsert_snapshot_blob_gc_candidate(id)
        .await
        .expect("mark");
    // The row lands (re-pin). A template row (session_id None) exercises
    // the same pin-by-row-existence; the FK would otherwise need a session.
    seed_snapshot_row_with_id(&meta, id, None).await;

    let r = run_one_snapshot_blob_sweep(meta.clone(), blob.clone(), &zero_grace(), SweepMode::Full)
        .await
        .expect("sweep");
    assert!(
        r.promote_repinned_skips >= 1,
        "the re-pinned candidate must be skipped, not deleted: {r:?}"
    );
    assert!(
        blob.exists(&state_blob_key(id)).await.unwrap(),
        "a snapshot whose row landed after marking must NOT lose its blobs"
    );
    // And the stale candidate row is cleared.
    let future = Utc::now() + chrono::Duration::hours(1);
    assert!(!meta
        .list_expired_snapshot_blob_gc_candidates(future, 10_000)
        .await
        .expect("list")
        .contains(&id));
}

/// Record a snapshot row with a caller-chosen id (the candidate was
/// marked for that exact id before the row existed).
async fn seed_snapshot_row_with_id(
    meta: &Arc<dyn MetadataStore>,
    id: SnapshotId,
    session_id: Option<SessionId>,
) {
    meta.record_snapshot(SnapshotRecord {
        id,
        session_id,
        host_id: None,
        image_version: "snapshot-blob-gc-fixture".into(),
        size_bytes: 0,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: None,
        memory_manifest: None,
        recoverable: true,
        aux_bundles: Vec::new(),
        events_cursor: None,
    })
    .await
    .expect("seed snapshot row");
}
