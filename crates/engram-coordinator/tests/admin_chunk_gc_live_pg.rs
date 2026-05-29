//! Live-Postgres integration tests for the ADR 0016 Phase C
//! chunk-GC sweep orchestrator. Exercises `run_one_sweep_inner`
//! against a real PG + LocalBlobStorage, with real manifests +
//! chunks materialized and a deliberately-orphaned chunk seeded
//! directly into the blob store.
//!
//! `#[ignore]`'d by default; requires Postgres reachable at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test admin_chunk_gc_live_pg -- --ignored
//! ```
//!
//! Coverage:
//! - `pin_set_covers_all_three_sources_and_dry_run_is_pure` —
//!   enabled image + live session manifest + recoverable snapshot
//!   all pin their chunks; a planted orphan ends up in the
//!   dry-run candidate count without any DB or blob mutation.
//! - `full_sweep_with_zero_grace_promotes_orphan_and_keeps_pinned`
//!   — promote-pass deletes the orphan but leaves pinned chunks
//!   intact. Idempotent on a clean second sweep.
//! - `nonzero_grace_protects_recent_candidates` — an orphan
//!   ends up in the candidate table but does NOT get deleted on
//!   the same sweep when grace>0.
//! - `non_recoverable_snapshots_do_not_pin` — a snapshot with
//!   `recoverable=false` does not protect its chunks from GC.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_chunk_store::{ChunkHash, ChunkRef, ChunkStore, Manifest, ManifestKind, ManifestRef};
use engram_coordinator::chunk_gc::{run_one_sweep_inner, ChunkGcConfig, SweepMode, SweepReport};
use engram_core::traits::{BlobStorage, MetadataStore};
use engram_core::types::registry::EnabledImage;
use engram_core::types::session::SessionMode;
use engram_core::types::{SandboxId, SessionSpec, SnapshotId, SnapshotRecord};
use engram_storage_local::LocalBlobStorage;
use uuid::Uuid;

struct TestRig {
    pg: Arc<engram_postgres::PostgresStore>,
    meta: Arc<dyn MetadataStore>,
    blob: Arc<dyn BlobStorage>,
    chunk_store: ChunkStore,
    _blob_dir: tempfile::TempDir,
}

async fn rig() -> Option<TestRig> {
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

    let blob_dir = tempfile::tempdir().expect("tempdir");
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(blob_dir.path().to_path_buf()));
    let chunk_store = ChunkStore::new(blob.clone());

    let pg = Arc::new(store);
    let rig = TestRig {
        pg: pg.clone(),
        meta: pg as Arc<dyn MetadataStore>,
        blob,
        chunk_store,
        _blob_dir: blob_dir,
    };

    // Defensive: wipe pin-set state from any prior test run that
    // may have panicked + leaked rows pointing at a now-gone
    // tempdir. Without this, `PinSet::collect` in the first
    // sweep fails with `ChunkStore(Blob(NotFound))` on the leaked
    // manifest_id. The wipe is broad (every enabled_image, every
    // snapshot) but it's a test DB — no concurrent state to
    // preserve, and CI's --test-threads=1 serializes tests.
    wipe_pin_set_state(&rig.pg).await;

    Some(rig)
}

/// Nuclear cleanup: remove every row that could pin a chunk in
/// `PinSet::collect`. Called at the start of every test so any
/// leaked state from a prior panicking run is cleared before this
/// test seeds its known state.
///
/// - `enabled_images`: all rows deleted (pin-set source #1).
/// - `sessions`: live_disk_manifest_* cleared so pin-set source
///   #2 returns empty. Session rows themselves stay (FK from
///   snapshots).
/// - `snapshots`: all rows deleted (pin-set sources #3 + #4).
/// - `chunk_gc_candidates`: cleared so stale candidate rows don't
///   leak into this test's promote-pass assertions.
/// - `chunk_generation`: bumped to mark the new state.
async fn wipe_pin_set_state(pg: &engram_postgres::PostgresStore) {
    let pool = pg.pool();
    sqlx::query("DELETE FROM chunk_gc_candidates")
        .execute(pool)
        .await
        .expect("wipe chunk_gc_candidates");
    // ADR 0020 P1: `enabled_images.base_snapshot_id` is a NOT NULL FK
    // into `snapshots`, so clear the referencing table FIRST or the
    // snapshots delete trips the FK constraint.
    sqlx::query("DELETE FROM enabled_images")
        .execute(pool)
        .await
        .expect("wipe enabled_images");
    sqlx::query("DELETE FROM snapshots")
        .execute(pool)
        .await
        .expect("wipe snapshots");
    sqlx::query(
        "UPDATE sessions
            SET sandbox_id                 = NULL,
                live_disk_manifest_id      = NULL,
                live_disk_manifest_version = NULL,
                live_disk_manifest_at      = NULL
          WHERE live_disk_manifest_id IS NOT NULL OR sandbox_id IS NOT NULL",
    )
    .execute(pool)
    .await
    .expect("clear sessions.live_disk_manifest_*");
    sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
        .execute(pool)
        .await
        .expect("bump chunk_generation");
}

/// Write a manifest of `chunks_bytes` to BlobStorage and return
/// its ManifestRef. Mirrors the seed_manifest helper in the
/// chunk-store unit tests but goes through the production
/// ChunkStore::put_chunk + put_manifest path so the on-disk
/// shape is exactly what the GC sweep will encounter.
async fn seed_manifest(
    store: &ChunkStore,
    chunks_bytes: &[&[u8]],
    kind: ManifestKind,
) -> ManifestRef {
    let chunk_size_bytes = kind.default_chunk_size();
    let total_bytes = (chunks_bytes.len() as u64) * chunk_size_bytes;
    let mut manifest = Manifest::empty(kind, total_bytes);
    for (i, bytes) in chunks_bytes.iter().enumerate() {
        let hash = store.put_chunk(bytes).await.expect("put chunk");
        manifest.chunks.push(ChunkRef {
            offset: (i as u64) * chunk_size_bytes,
            hash,
        });
    }
    let r = ManifestRef::new();
    store
        .put_manifest(r, &manifest)
        .await
        .expect("put manifest");
    r
}

/// Plant an orphan chunk under `chunks/sha256/<2>/<62>` with
/// content unique to this test invocation. Returns its hash so
/// assertions can compare against the swept candidate set.
async fn plant_orphan(blob: &dyn BlobStorage) -> ChunkHash {
    let unique = format!("orphan-{}", Uuid::new_v4());
    let hash = ChunkHash::of(unique.as_bytes());
    blob.put(&hash.storage_key(), bytes::Bytes::from(unique))
        .await
        .expect("plant orphan chunk");
    hash
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn pin_set_covers_all_three_sources_and_dry_run_is_pure() {
    let Some(rig) = rig().await else { return };

    // ---- enabled image with chunked-disk manifest ----
    let img_disk = seed_manifest(&rig.chunk_store, &[b"img-a", b"img-b"], ManifestKind::Disk).await;
    let image_uri = format!("phase-c-pin-test:warm-{}", Uuid::new_v4());
    // ADR 0020: enabled_images.base_snapshot_id is NOT NULL + FK to
    // snapshots(id) (migration 0038); seed a throwaway template snapshot.
    let base_snap = SnapshotId::new();
    rig.meta
        .record_snapshot(SnapshotRecord {
            id: base_snap,
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
    rig.meta
        .upsert_enabled_image(EnabledImage {
            id: Uuid::new_v4(),
            image_uri: image_uri.clone(),
            manifest_toml: "image = { uri = \"phase-c\" }\n".into(),
            manifest_digest: format!("sha256:{:064x}", 1u32),
            disk_manifest: Some(img_disk),
            base_snapshot_id: Some(base_snap),
            base_snapshot_disk_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            }),
            base_snapshot_memory_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            }),
            last_refreshed_at: Utc::now(),
            created_at: Utc::now(),
            updated_at: None,
            soft_deleted_at: None,
        })
        .await
        .expect("upsert enabled image");

    // ---- session with live_disk_manifest ----
    let session_disk = seed_manifest(
        &rig.chunk_store,
        &[b"sess-a", b"sess-b"],
        ManifestKind::Disk,
    )
    .await;
    let session_id = rig
        .meta
        .create_session(SessionSpec {
            image: image_uri.clone(),
            mode: SessionMode::Agent,
            user_id: None,
        })
        .await
        .expect("create session");
    let sandbox_id = SandboxId::new();
    rig.meta
        .assign_session_sandbox(session_id, Some(sandbox_id))
        .await
        .expect("assign sandbox");
    rig.meta
        .update_live_disk_manifest(session_id, sandbox_id, session_disk)
        .await
        .expect("update live manifest");

    // ---- recoverable snapshot with disk + memory manifests ----
    let snap_disk = seed_manifest(&rig.chunk_store, &[b"snap-d-a"], ManifestKind::Disk).await;
    let snap_mem = seed_manifest(&rig.chunk_store, &[b"snap-m-a"], ManifestKind::Memory).await;
    rig.meta
        .record_snapshot(SnapshotRecord {
            id: SnapshotId::new(),
            session_id: Some(session_id),
            host_id: None,
            image_version: "warm-1".into(),
            size_bytes: 1024,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
            disk_manifest: Some(snap_disk),
            memory_manifest: Some(snap_mem),
            recoverable: true,
        })
        .await
        .expect("record snapshot");

    // ---- orphan chunk ----
    let orphan_hash = plant_orphan(rig.blob.as_ref()).await;

    // ---- dry-run sweep ----
    let cfg = ChunkGcConfig {
        enabled: true,
        interval: Duration::from_secs(3600),
        grace_period: Duration::from_secs(0),
        max_restart_attempts: 3,
        promote_batch_size: 10_000,
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::DryRun,
    )
    .await
    .expect("dry-run sweep");

    // Assertions: pin set covers all four manifests' chunks (2 + 2
    // + 1 + 1 = 6 distinct ChunkHashes). The orphan is the lone
    // candidate. Dry-run promotes nothing.
    assert_eq!(
        report.pin_set_size, 6,
        "pin set should cover all 4 manifests' chunks"
    );
    assert_eq!(
        report.candidates_marked, 1,
        "only the orphan should classify as a candidate"
    );
    assert_eq!(report.promoted_deletes, 0, "dry-run must not delete");
    assert_eq!(report.promote_delete_errors, 0);

    // Dry-run must not touch the candidate table.
    let candidates = rig
        .meta
        .list_gc_candidates(100, None)
        .await
        .expect("list candidates");
    assert!(
        !candidates
            .iter()
            .any(|r| r.content_hash == *orphan_hash.as_bytes()),
        "DryRun must NOT upsert the orphan into chunk_gc_candidates",
    );

    // Orphan still in BlobStorage after dry-run.
    let still_present = rig
        .blob
        .exists(&orphan_hash.storage_key())
        .await
        .expect("exists");
    assert!(still_present, "DryRun must NOT delete the orphan blob");
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn full_sweep_with_zero_grace_promotes_orphan_and_keeps_pinned() {
    let Some(rig) = rig().await else { return };

    // One pinned manifest + one orphan.
    let pinned_mref = seed_manifest(
        &rig.chunk_store,
        &[b"keepme-a", b"keepme-b"],
        ManifestKind::Disk,
    )
    .await;
    let image_uri = format!("phase-c-promote-test:warm-{}", Uuid::new_v4());
    // ADR 0020: enabled_images.base_snapshot_id is NOT NULL + FK to
    // snapshots(id) (migration 0038); seed a throwaway template snapshot.
    let base_snap = SnapshotId::new();
    rig.meta
        .record_snapshot(SnapshotRecord {
            id: base_snap,
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
    rig.meta
        .upsert_enabled_image(EnabledImage {
            id: Uuid::new_v4(),
            image_uri: image_uri.clone(),
            manifest_toml: "image = { uri = \"phase-c\" }\n".into(),
            manifest_digest: format!("sha256:{:064x}", 2u32),
            disk_manifest: Some(pinned_mref),
            base_snapshot_id: Some(base_snap),
            base_snapshot_disk_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            }),
            base_snapshot_memory_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            }),
            last_refreshed_at: Utc::now(),
            created_at: Utc::now(),
            updated_at: None,
            soft_deleted_at: None,
        })
        .await
        .expect("upsert enabled image");

    let orphan_hash = plant_orphan(rig.blob.as_ref()).await;

    // Fetch the actual chunk hashes from the pinned manifest so we
    // can assert they survived.
    let pinned_manifest = rig
        .chunk_store
        .get_manifest(pinned_mref)
        .await
        .expect("read pinned manifest");
    let pinned_hashes: Vec<ChunkHash> = pinned_manifest.chunks.iter().map(|c| c.hash).collect();

    // grace_period=0 → orphan promotes on the same sweep.
    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        ..ChunkGcConfig::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
    )
    .await
    .expect("full sweep");

    assert_eq!(report.candidates_marked, 1, "orphan should classify");
    assert_eq!(report.promoted_deletes, 1, "orphan should promote");
    assert_eq!(report.promote_delete_errors, 0);

    // Orphan blob is gone.
    let still_present = rig
        .blob
        .exists(&orphan_hash.storage_key())
        .await
        .expect("exists check");
    assert!(!still_present, "orphan blob must be deleted after promote");

    // Pinned blobs still present.
    for h in &pinned_hashes {
        let present = rig.blob.exists(&h.storage_key()).await.expect("exists");
        assert!(present, "pinned chunk {h} must NOT be deleted");
    }

    // Candidate table cleaned up post-promote.
    let candidates = rig
        .meta
        .list_gc_candidates(100, None)
        .await
        .expect("list candidates");
    assert!(
        !candidates
            .iter()
            .any(|r| r.content_hash == *orphan_hash.as_bytes()),
        "candidate row must be cleaned up after successful promote",
    );

    // Idempotent: second sweep finds nothing to do.
    let report_again = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
    )
    .await
    .expect("second sweep");
    assert_eq!(report_again.candidates_marked, 0, "no new orphans");
    assert_eq!(report_again.promoted_deletes, 0, "nothing to promote");
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn nonzero_grace_protects_recent_candidates() {
    let Some(rig) = rig().await else { return };

    let orphan_hash = plant_orphan(rig.blob.as_ref()).await;

    // grace=1h — candidate ends up in the table but does NOT
    // delete on this sweep.
    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(3600),
        ..ChunkGcConfig::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
    )
    .await
    .expect("full sweep");

    assert_eq!(report.candidates_marked, 1);
    assert_eq!(
        report.promoted_deletes, 0,
        "grace window protects the just-marked candidate"
    );

    // Candidate present.
    let candidates = rig
        .meta
        .list_gc_candidates(100, None)
        .await
        .expect("list candidates");
    let found = candidates
        .iter()
        .any(|r| r.content_hash == *orphan_hash.as_bytes());
    assert!(found, "candidate row must persist for the grace window");

    // Orphan blob still present.
    let still_present = rig
        .blob
        .exists(&orphan_hash.storage_key())
        .await
        .expect("exists");
    assert!(still_present, "orphan must NOT be deleted within grace");

    // Now run a follow-up sweep with grace=0 — same candidate
    // promotes (proves first_seen_at didn't get bumped on re-
    // sighting, which would extend the window).
    let cfg_zero = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        ..ChunkGcConfig::default()
    };
    let report_zero = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg_zero,
        SweepMode::Full,
    )
    .await
    .expect("second sweep zero-grace");
    assert_eq!(
        report_zero.promoted_deletes, 1,
        "grace=0 must let the previously-protected candidate promote"
    );

    let still_present = rig
        .blob
        .exists(&orphan_hash.storage_key())
        .await
        .expect("exists");
    assert!(!still_present, "orphan must be gone after grace=0 sweep");
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn non_recoverable_snapshots_do_not_pin() {
    let Some(rig) = rig().await else { return };

    // Materialize a manifest's chunks and reference them ONLY via
    // a non-recoverable snapshot. The chunks should NOT be in the
    // pin set; they should be marked as candidates.
    let unrec_mref = seed_manifest(&rig.chunk_store, &[b"unrec"], ManifestKind::Disk).await;
    let unrec_manifest = rig
        .chunk_store
        .get_manifest(unrec_mref)
        .await
        .expect("read manifest");
    let unrec_hashes: Vec<ChunkHash> = unrec_manifest.chunks.iter().map(|c| c.hash).collect();
    assert_eq!(unrec_hashes.len(), 1);

    let session_id = rig
        .meta
        .create_session(SessionSpec {
            image: format!("phase-c-unrec-test:warm-{}", Uuid::new_v4()),
            mode: SessionMode::Agent,
            user_id: None,
        })
        .await
        .expect("create session");

    rig.meta
        .record_snapshot(SnapshotRecord {
            id: SnapshotId::new(),
            session_id: Some(session_id),
            host_id: None,
            image_version: "warm-1".into(),
            size_bytes: 1024,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
            disk_manifest: Some(unrec_mref),
            memory_manifest: None,
            recoverable: false, // ← the load-bearing field
        })
        .await
        .expect("record snapshot");

    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        ..ChunkGcConfig::default()
    };
    let report: SweepReport = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
    )
    .await
    .expect("sweep");

    // Pin set excludes the non-recoverable snapshot's chunks.
    // candidates_marked includes both the unrec chunk and any other
    // orphans this DB happens to have (other tests share the DB).
    // The targeted assertion: the unrec chunk WAS deleted.
    assert!(
        report.candidates_marked >= 1,
        "non-recoverable snapshot's chunk must classify as candidate"
    );
    let unrec_hash = unrec_hashes[0];
    let still_present = rig
        .blob
        .exists(&unrec_hash.storage_key())
        .await
        .expect("exists");
    assert!(
        !still_present,
        "chunk pinned only by a non-recoverable snapshot must be deleted",
    );
}
