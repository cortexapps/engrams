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
//!   enabled image (rootfs + base-snapshot disk/memory, ADR 0022
//!   sources #5/#6) + live session manifest + recoverable snapshot
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
//! - `base_snapshot_memfile_pinned_even_when_snapshot_not_recoverable`
//!   — ADR 0022 sources #5/#6 pin the per-template memfile + rootfs
//!   via the enabled_images row independent of the base snapshot
//!   row's `recoverable` flag.
//! - `mark_failure_still_promotes_already_expired_candidates` — a
//!   failing `list_prefix` degrades the sweep instead of voiding it;
//!   the promote pass still reaps candidates an earlier sweep marked.
//! - `promote_drains_multiple_batches_in_one_sweep` — the promote
//!   pass pages until the backlog clears, not one page per tick.
//! - `promote_respects_the_per_sweep_cap` — `promote_max_per_sweep`
//!   bounds one sweep and leaves the remainder queued.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

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

/// ADR 0098 D1: the GC sweeps take an injected clock; live tests run on
/// the real one.
fn system_clock() -> std::sync::Arc<dyn engram_core::traits::Clock> {
    std::sync::Arc::new(engram_core::traits::SystemClock::new())
}

struct TestRig {
    meta: Arc<dyn MetadataStore>,
    blob: Arc<dyn BlobStorage>,
    chunk_store: ChunkStore,
    _blob_dir: tempfile::TempDir,
}

async fn rig() -> Option<TestRig> {
    // Each test gets its own template-cloned database (ADR 0099 H1), so
    // there is no leaked pin-set state from prior runs or sibling tests
    // to wipe — the rig starts from a freshly-migrated empty schema.
    let db = engram_testkit::pg::fresh_db().await?;

    let blob_dir = tempfile::tempdir().expect("tempdir");
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(blob_dir.path().to_path_buf()));
    let chunk_store = ChunkStore::new(blob.clone());

    Some(TestRig {
        meta: Arc::new(db.store),
        blob,
        chunk_store,
        _blob_dir: blob_dir,
    })
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
    // ADR 0022 sources #5/#6: the base snapshot's own memory + disk
    // manifests, pinned via enabled_images.base_snapshot_*. Real chunks so
    // PinSet::collect can fetch them (seeding random unbacked refs here was
    // a latent bug that became live once #5/#6 fetch these columns).
    let base_disk = seed_manifest(&rig.chunk_store, &[b"base-d"], ManifestKind::Disk).await;
    let base_mem = seed_manifest(&rig.chunk_store, &[b"base-m"], ManifestKind::Memory).await;
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
            aux_bundles: vec![],
            events_cursor: None,
            fc_snapshot_version: None,
        })
        .await
        .expect("seed base snapshot");
    rig.meta
        .upsert_enabled_image(EnabledImage {
            id: Uuid::new_v4(),
            image_uri: image_uri.clone(),
            image_config: engram_core::types::image::ImageConfig {
                name: "test".into(),
                ..Default::default()
            },
            oci_defaults: Default::default(),
            manifest_digest: format!("sha256:{:064x}", 1u32),
            disk_manifest: Some(img_disk),
            base_snapshot_id: Some(base_snap),
            base_snapshot_disk_manifest: Some(base_disk),
            base_snapshot_memory_manifest: Some(base_mem),
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
        })
        .await
        .expect("create session");
    let sandbox_id = SandboxId::new();
    // 0108: bind via the production fused path, never on a Pending row.
    rig.meta
        .transition_session_created(session_id, sandbox_id)
        .await
        .expect("assign sandbox (fused Pending → Created)");
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
            aux_bundles: vec![],
            events_cursor: None,
            fc_snapshot_version: None,
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
        ..Default::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::DryRun,
        &system_clock(),
        0,
    )
    .await
    .expect("dry-run sweep");

    // Assertions: pin set covers all six sources' chunks: image disk (2)
    // + base-snapshot disk #6 (1) + base-snapshot memory #5 (1) + session
    // live (2) + recoverable snapshot disk (1) + memory (1) = 8 distinct
    // ChunkHashes. The orphan is the lone candidate. Dry-run promotes nothing.
    assert_eq!(
        report.pin_set_size, 8,
        "pin set should cover all 6 manifests' chunks"
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
    // ADR 0022 sources #5/#6: real base-snapshot manifests (not random
    // unbacked refs) so PinSet::collect can fetch them.
    let base_disk = seed_manifest(&rig.chunk_store, &[b"promote-base-d"], ManifestKind::Disk).await;
    let base_mem =
        seed_manifest(&rig.chunk_store, &[b"promote-base-m"], ManifestKind::Memory).await;
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
            aux_bundles: vec![],
            events_cursor: None,
            fc_snapshot_version: None,
        })
        .await
        .expect("seed base snapshot");
    rig.meta
        .upsert_enabled_image(EnabledImage {
            id: Uuid::new_v4(),
            image_uri: image_uri.clone(),
            image_config: engram_core::types::image::ImageConfig {
                name: "test".into(),
                ..Default::default()
            },
            oci_defaults: Default::default(),
            manifest_digest: format!("sha256:{:064x}", 2u32),
            disk_manifest: Some(pinned_mref),
            base_snapshot_id: Some(base_snap),
            base_snapshot_disk_manifest: Some(base_disk),
            base_snapshot_memory_manifest: Some(base_mem),
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
        &system_clock(),
        0,
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
        &system_clock(),
        0,
    )
    .await
    .expect("second sweep");
    assert_eq!(report_again.candidates_marked, 0, "no new orphans");
    assert_eq!(report_again.promoted_deletes, 0, "nothing to promote");
}

/// ADR 0016 Phase C durability regression (the wedged-session bug): a
/// chunk marked a candidate while transiently unpinned, then RE-PINNED
/// before the grace elapses (e.g. an image refresh re-referencing a
/// shared base-memory chunk), must NOT be deleted by the promote pass.
/// `first_seen_at` is sticky and classification never clears a re-pinned
/// candidate's row, so the promote pass re-verifies the live pin set and
/// skips + clears it. Without the re-check the live chunk's blob is wrongly
/// deleted and every reader of the pinning manifest 404s on first fault.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn promote_skips_candidate_that_became_repinned() {
    let Some(rig) = rig().await else { return };

    // A base-snapshot MEMORY manifest referencing one chunk — the exact
    // shape that wedged prod (a reaped base-memfile chunk). seed_manifest
    // puts the chunk; its content hash is ChunkHash::of(bytes).
    let target_bytes: &[u8] = b"repin-target-base-memory-chunk";
    let base_mem = seed_manifest(&rig.chunk_store, &[target_bytes], ManifestKind::Memory).await;
    let target_hash = ChunkHash::of(target_bytes);

    // A prior sweep marked it a candidate while it was (transiently)
    // unpinned; the sticky first_seen_at is now in the past.
    rig.meta
        .upsert_chunk_gc_candidate(*target_hash.as_bytes())
        .await
        .expect("mark candidate");

    // The chunk is RE-PINNED: an enabled image's base snapshot now
    // references it via base_snapshot_memory_manifest. Classification will
    // skip it as pinned but never clears the stale candidate row.
    let base_disk =
        seed_manifest(&rig.chunk_store, &[b"repin-base-disk"], ManifestKind::Disk).await;
    let img_disk = seed_manifest(&rig.chunk_store, &[b"repin-img-disk"], ManifestKind::Disk).await;
    let image_uri = format!("phase-c-repin-test:warm-{}", Uuid::new_v4());
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
            aux_bundles: vec![],
            events_cursor: None,
            fc_snapshot_version: None,
        })
        .await
        .expect("seed base snapshot");
    rig.meta
        .upsert_enabled_image(EnabledImage {
            id: Uuid::new_v4(),
            image_uri: image_uri.clone(),
            image_config: engram_core::types::image::ImageConfig {
                name: "test".into(),
                ..Default::default()
            },
            oci_defaults: Default::default(),
            manifest_digest: format!("sha256:{:064x}", 3u32),
            disk_manifest: Some(img_disk),
            base_snapshot_id: Some(base_snap),
            base_snapshot_disk_manifest: Some(base_disk),
            base_snapshot_memory_manifest: Some(base_mem),
            last_refreshed_at: Utc::now(),
            created_at: Utc::now(),
            updated_at: None,
            soft_deleted_at: None,
        })
        .await
        .expect("upsert enabled image");

    // grace=0 → the candidate row is "expired" on this sweep, so promote
    // considers it for deletion — and must skip it because it's pinned.
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
        &system_clock(),
        0,
    )
    .await
    .expect("sweep");

    assert!(
        report.promote_repinned_skips >= 1,
        "the re-pinned candidate must be skipped, not deleted: {report:?}"
    );
    assert_eq!(
        report.promoted_deletes, 0,
        "nothing was genuinely unpinned, so nothing should be deleted: {report:?}"
    );

    // The load-bearing assertion: the live chunk's blob survives (the bug
    // deletes it here, 404ing the base-snapshot memfile prefetch).
    assert!(
        rig.blob
            .exists(&target_hash.storage_key())
            .await
            .expect("exists"),
        "a re-pinned chunk's blob must NOT be deleted by the promote pass",
    );

    // The stale candidate row is cleared so it isn't re-evaluated forever.
    let candidates = rig
        .meta
        .list_gc_candidates(100, None)
        .await
        .expect("list candidates");
    assert!(
        !candidates
            .iter()
            .any(|r| r.content_hash == *target_hash.as_bytes()),
        "the rescued candidate's stale row must be cleared",
    );
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
        &system_clock(),
        0,
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
        &system_clock(),
        0,
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
            aux_bundles: vec![],
            events_cursor: None,
            fc_snapshot_version: None,
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
        &system_clock(),
        0,
    )
    .await
    .expect("sweep");

    // Pin set excludes the non-recoverable snapshot's chunks.
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

/// ADR 0022 Option A: the per-template base memfile (+ its rootfs) must
/// stay pinned via the enabled_images row even when the base snapshot it
/// came from is NOT recoverable — i.e. sources #5/#6 are independent of
/// the `recoverable` flag, exactly as source #1 is. This is the inverse
/// of `non_recoverable_snapshots_do_not_pin`: there the only reference was
/// a non-recoverable snapshot (→ deleted); here the enabled_images
/// base_snapshot_* columns also reference the manifests (→ pinned).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn base_snapshot_memfile_pinned_even_when_snapshot_not_recoverable() {
    let Some(rig) = rig().await else { return };

    // The memfile backing (memory) + rootfs (disk) of the base snapshot.
    let memfile_mref =
        seed_manifest(&rig.chunk_store, &[b"memfile-chunk"], ManifestKind::Memory).await;
    let rootfs_mref = seed_manifest(&rig.chunk_store, &[b"rootfs-chunk"], ManifestKind::Disk).await;
    let memfile_hash = rig
        .chunk_store
        .get_manifest(memfile_mref)
        .await
        .expect("read memfile manifest")
        .chunks[0]
        .hash;
    let rootfs_hash = rig
        .chunk_store
        .get_manifest(rootfs_mref)
        .await
        .expect("read rootfs manifest")
        .chunks[0]
        .hash;

    // Base snapshot recorded NON-recoverable, with no manifests of its own,
    // so sources #3/#4 cannot pin memfile/rootfs.
    let base_snap = SnapshotId::new();
    rig.meta
        .record_snapshot(SnapshotRecord {
            id: base_snap,
            session_id: None,
            host_id: None,
            image_version: "base-snapshot-nonrec".into(),
            size_bytes: 0,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
            disk_manifest: None,
            memory_manifest: None,
            recoverable: false, // ← the load-bearing field
            aux_bundles: vec![],
            events_cursor: None,
            fc_snapshot_version: None,
        })
        .await
        .expect("seed non-recoverable base snapshot");

    // The enabled image references the memfile/rootfs ONLY via the
    // base_snapshot_* columns (disk_manifest = None → source #1 pins
    // nothing). So the only pin path is sources #5/#6.
    rig.meta
        .upsert_enabled_image(EnabledImage {
            id: Uuid::new_v4(),
            image_uri: format!("adr-0022-memfile-pin:warm-{}", Uuid::new_v4()),
            image_config: engram_core::types::image::ImageConfig {
                name: "test".into(),
                ..Default::default()
            },
            oci_defaults: Default::default(),
            manifest_digest: format!("sha256:{:064x}", 22u32),
            disk_manifest: None,
            base_snapshot_id: Some(base_snap),
            base_snapshot_disk_manifest: Some(rootfs_mref),
            base_snapshot_memory_manifest: Some(memfile_mref),
            last_refreshed_at: Utc::now(),
            created_at: Utc::now(),
            updated_at: None,
            soft_deleted_at: None,
        })
        .await
        .expect("upsert enabled image");

    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        ..ChunkGcConfig::default()
    };
    run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
        &system_clock(),
        0,
    )
    .await
    .expect("sweep");

    // Both survive: pinned via enabled_images base_snapshot_* (sources
    // #5/#6) despite the base snapshot being non-recoverable.
    for (h, what) in [(memfile_hash, "memfile"), (rootfs_hash, "rootfs")] {
        let present = rig.blob.exists(&h.storage_key()).await.expect("exists");
        assert!(
            present,
            "{what} chunk must stay pinned via enabled_images base_snapshot_* \
             even though its base snapshot is non-recoverable",
        );
    }
}

// ---------------------------------------------------------------------------
// Mark/promote independence + drain-loop regressions.
// ---------------------------------------------------------------------------

/// The mark pass needs a full `list_prefix`; the promote pass needs only
/// rows an earlier sweep wrote. Chaining promote behind the mark pass's
/// `?` meant one failing list disabled deletion entirely — in prod
/// `list_prefix` exceeded its 300s deadline on every sweep from
/// 2026-07-16, stranding 8.9M expired candidates that needed no listing
/// to delete. A mark failure must degrade the sweep, never void it.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn mark_failure_still_promotes_already_expired_candidates() {
    let Some(rig) = rig().await else { return };

    let orphan = plant_orphan(rig.blob.as_ref()).await;

    // Sweep 1: healthy listing, non-zero grace — the orphan is recorded as
    // a candidate but not yet deletable.
    let marking = ChunkGcConfig {
        grace_period: Duration::from_secs(3600),
        ..Default::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &marking,
        SweepMode::Full,
        &system_clock(),
        0,
    )
    .await
    .expect("marking sweep");
    assert!(
        report.mark_error.is_none(),
        "sweep 1 listing should succeed"
    );
    assert_eq!(report.candidates_marked, 1, "orphan marked");
    assert_eq!(report.promoted_deletes, 0, "grace protects it this sweep");
    assert!(
        rig.blob
            .exists(&orphan.storage_key())
            .await
            .expect("exists"),
        "orphan still present after the marking sweep"
    );

    // Sweep 2: listing is broken exactly the way prod's was, and the grace
    // window has elapsed. The mark pass fails; the promote pass must still
    // reap the candidate sweep 1 recorded.
    let (faulty, counters) = engram_testkit::storage::FaultyBlobStorage::arc(
        rig.blob.clone(),
        engram_testkit::storage::FaultPlan::new().fail_list(
            "chunks/",
            engram_testkit::storage::InjectedError::Sdk("attempt deadline 300s exceeded".into()),
        ),
    );
    let faulty_store = ChunkStore::new(faulty.clone());
    let promoting = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        ..Default::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        faulty.clone(),
        &faulty_store,
        &promoting,
        SweepMode::Full,
        &system_clock(),
        0,
    )
    .await
    .expect("a failed mark pass must not fail the sweep");

    assert!(
        report.mark_error.is_some(),
        "the injected list fault should be reported, not swallowed"
    );
    // Once per shard in the first concurrent group, not once overall:
    // the mark pass walks `shard_concurrency` shards at a time and every
    // one of them hits the injected fault.
    assert!(
        counters.lists_faulted() >= 1,
        "the list fault actually fired"
    );
    assert_eq!(
        report.promoted_deletes, 1,
        "promote must run on the existing candidate despite the mark failure"
    );
    assert!(
        !rig.blob
            .exists(&orphan.storage_key())
            .await
            .expect("exists"),
        "orphan blob deleted by the promote pass"
    );
    assert_eq!(
        rig.meta.count_gc_candidates().await.expect("count"),
        0,
        "candidate row cleared"
    );
}

/// One page per sweep cannot drain a backlog: at 10k/hour the 8.9M
/// candidates found in prod would need 37 days. The promote pass drains
/// in pages until the backlog clears or the per-sweep cap is hit.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn promote_drains_multiple_batches_in_one_sweep() {
    let Some(rig) = rig().await else { return };

    let mut orphans = Vec::new();
    for _ in 0..25 {
        orphans.push(plant_orphan(rig.blob.as_ref()).await);
    }

    // Batch size 10 over 25 orphans: a single-page promote would leave 15.
    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        promote_batch_size: 10,
        ..Default::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
        &system_clock(),
        0,
    )
    .await
    .expect("draining sweep");

    assert_eq!(report.candidates_marked, 25);
    assert_eq!(
        report.promoted_deletes, 25,
        "every expired candidate drained in one sweep, not just the first page"
    );
    assert_eq!(
        rig.meta.count_gc_candidates().await.expect("count"),
        0,
        "candidate table drained"
    );
    for o in &orphans {
        assert!(
            !rig.blob.exists(&o.storage_key()).await.expect("exists"),
            "orphan {} deleted",
            o.to_hex()
        );
    }
}

/// The promote pass is gated by a WALL-CLOCK budget, not a row cap. A
/// cap made deletion the binding constraint and could leave a backlog
/// stuck for weeks; a spent budget must stop the drain cleanly and leave
/// the backlog for the next tick.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn promote_respects_the_wall_clock_budget() {
    let Some(rig) = rig().await else { return };

    for _ in 0..20 {
        plant_orphan(rig.blob.as_ref()).await;
    }

    // Zero budget: the pass must decline to delete anything even though
    // every candidate is past its (zero) grace.
    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(0),
        promote_budget: Duration::from_secs(0),
        ..Default::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
        &system_clock(),
        0,
    )
    .await
    .expect("budgeted sweep");

    assert_eq!(report.candidates_marked, 20, "marking still happened");
    assert_eq!(
        report.promoted_deletes, 0,
        "a spent promote budget deletes nothing"
    );
    assert_eq!(
        rig.meta.count_gc_candidates().await.expect("count"),
        20,
        "the backlog is left for the next tick"
    );
}

/// The mark pass walks the key space in hash-prefix shards and resumes
/// from a cursor. Marking the whole space in one tick is not bounded
/// work — it grows with garbage, and in 2026-08 it outgrew a tick
/// entirely (46h without finishing). A budget that stops the walk must
/// still leave a cursor that advances.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn mark_pass_resumes_from_the_shard_cursor() {
    let Some(rig) = rig().await else { return };

    let mut orphans = Vec::new();
    for _ in 0..40 {
        orphans.push(plant_orphan(rig.blob.as_ref()).await);
    }

    // One shard group per tick, so a single sweep cannot cover all 256.
    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(3600),
        shard_concurrency: 8,
        ..Default::default()
    };

    // Walk the whole space across successive ticks, threading the cursor
    // exactly as the scheduled sweep does.
    let mut start = 0u32;
    let mut ticks = 0;
    let mut total_marked = 0usize;
    loop {
        let report = run_one_sweep_inner(
            rig.meta.clone(),
            rig.blob.clone(),
            &rig.chunk_store,
            &cfg,
            SweepMode::Full,
            &system_clock(),
            start,
        )
        .await
        .expect("sharded sweep");
        total_marked += report.candidates_marked;
        start = report.next_shard;
        ticks += 1;
        if report.full_cycle_completed || ticks > 64 {
            break;
        }
    }

    assert!(
        ticks <= 64,
        "the cursor must advance and the walk must terminate"
    );
    assert_eq!(
        total_marked, 40,
        "every orphan is found exactly once across the full cycle"
    );
    assert_eq!(
        rig.meta.count_gc_candidates().await.expect("count"),
        40,
        "all 40 recorded, none double-counted"
    );
}

/// A shard walk that covers the whole space in one tick reports a
/// completed cycle and wraps the cursor back to 0.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn full_cycle_wraps_the_cursor() {
    let Some(rig) = rig().await else { return };
    plant_orphan(rig.blob.as_ref()).await;

    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(3600),
        shard_concurrency: 64,
        ..Default::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
        &system_clock(),
        0,
    )
    .await
    .expect("full-cycle sweep");

    assert!(report.full_cycle_completed);
    assert_eq!(report.shards_scanned, 256);
    assert_eq!(report.next_shard, 0, "wrapped back to the start");
    assert_eq!(report.candidates_marked, 1);
}

/// The mark pass walks the chunk space one page at a time. Buffering
/// the whole listing was the prod failure: at ~110M keys every
/// `list_prefix` blew its 300s deadline, so no sweep ever classified
/// anything after 2026-07-16. A page-size below the key count must
/// still classify every chunk.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn mark_pass_walks_every_page_of_the_chunk_space() {
    let Some(rig) = rig().await else { return };

    let mut orphans = Vec::new();
    for _ in 0..37 {
        orphans.push(plant_orphan(rig.blob.as_ref()).await);
    }

    // 37 chunks at 5 keys per page = 8 pages. A single-page mark pass
    // would see 5.
    let cfg = ChunkGcConfig {
        grace_period: Duration::from_secs(3600),
        list_page_size: 5,
        ..Default::default()
    };
    let report = run_one_sweep_inner(
        rig.meta.clone(),
        rig.blob.clone(),
        &rig.chunk_store,
        &cfg,
        SweepMode::Full,
        &system_clock(),
        0,
    )
    .await
    .expect("paged mark sweep");

    assert!(report.mark_error.is_none());
    assert_eq!(
        report.listed_chunks, 37,
        "every page of the chunk space was walked"
    );
    assert_eq!(
        report.candidates_marked, 37,
        "every unpinned chunk was marked, not just the first page"
    );
    assert_eq!(
        rig.meta.count_gc_candidates().await.expect("count"),
        37,
        "all candidates recorded in PG"
    );
}
