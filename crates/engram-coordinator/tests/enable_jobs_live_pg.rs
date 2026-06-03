//! Live-Postgres tests for the ADR 0036 `enable_jobs` MetadataStore
//! surface: create-or-get dedup (one active job per URI), lease-based
//! claiming (multi-coordinator safety + expiry steal), progress
//! checkpoints, failure bookkeeping, and the admin retry transition.
//!
//! `#[ignore]`'d by default; requires Postgres reachable at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test enable_jobs_live_pg -- --ignored
//! ```

use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{EnableJobState, EnabledImage};
use engram_core::{MetaError, SnapshotId};
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

fn unique_uri(tag: &str) -> String {
    format!("test-registry.local/enable-jobs/{tag}:{}", Uuid::new_v4())
}

#[tokio::test]
#[ignore]
async fn create_or_get_dedups_active_jobs_per_uri() {
    let Some(meta) = connect().await else { return };
    let uri = unique_uri("dedup");

    let a = meta
        .create_or_get_enable_job(&uri, Some("sha256:digest-a"))
        .await
        .expect("create");
    assert_eq!(a.state, EnableJobState::Pending);
    assert_eq!(a.image_uri, uri);
    assert_eq!(a.chunks_done, 0);

    // Re-POST while in flight → same job, even with a moved digest.
    let b = meta
        .create_or_get_enable_job(&uri, Some("sha256:digest-b"))
        .await
        .expect("re-create");
    assert_eq!(b.id, a.id, "active job must be returned, not duplicated");

    // Terminal job → a fresh enable inserts a NEW row.
    meta.set_enable_job_state(a.id, EnableJobState::Ready)
        .await
        .expect("ready");
    let c = meta
        .create_or_get_enable_job(&uri, None)
        .await
        .expect("create after terminal");
    assert_ne!(c.id, a.id, "terminal jobs don't block a new enable");
}

#[tokio::test]
#[ignore]
async fn claim_is_exclusive_until_lease_expires() {
    let Some(meta) = connect().await else { return };
    let uri = unique_uri("claim");
    let job = meta
        .create_or_get_enable_job(&uri, None)
        .await
        .expect("create");

    // Pod A claims with a long lease.
    let claimed = meta
        .claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim a");
    assert!(
        claimed.iter().any(|j| j.id == job.id),
        "pod-a should claim the new job"
    );

    // Pod B sweeps immediately: the job is leased, not claimable.
    let claimed_b = meta
        .claim_enable_jobs("pod-b", 300, 50)
        .await
        .expect("claim b");
    assert!(
        !claimed_b.iter().any(|j| j.id == job.id),
        "a live lease must not be stolen"
    );

    // With a 0-second lease every claim is instantly expired — pod B
    // re-claims (the crashed-pod recovery path).
    let claimed_b2 = meta
        .claim_enable_jobs("pod-b", 0, 50)
        .await
        .expect("claim b2");
    assert!(
        claimed_b2.iter().any(|j| j.id == job.id),
        "an expired lease must be re-claimable by a peer"
    );

    // Cleanup: park the job in a terminal state so later sweeps in
    // other tests don't pick it up.
    meta.set_enable_job_state(job.id, EnableJobState::Failed)
        .await
        .expect("park");
}

#[tokio::test]
#[ignore]
async fn progress_state_failure_and_retry_round_trip() {
    let Some(meta) = connect().await else { return };
    let uri = unique_uri("lifecycle");
    let job = meta
        .create_or_get_enable_job(&uri, None)
        .await
        .expect("create");

    // Progress: total stamped once, done advances.
    meta.update_enable_job_progress(job.id, 0, Some(625))
        .await
        .expect("stamp total");
    meta.update_enable_job_progress(job.id, 100, None)
        .await
        .expect("checkpoint");
    let got = meta
        .get_enable_job(job.id)
        .await
        .expect("get")
        .expect("some");
    assert_eq!(got.chunks_total, Some(625));
    assert_eq!(got.chunks_done, 100);

    // State advance.
    meta.set_enable_job_state(job.id, EnableJobState::Materializing)
        .await
        .expect("materializing");
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.state, EnableJobState::Materializing);

    // Failures bump attempts and store the error.
    let a1 = meta
        .record_enable_job_failure(job.id, "registry 429")
        .await
        .expect("fail 1");
    let a2 = meta
        .record_enable_job_failure(job.id, "registry 503")
        .await
        .expect("fail 2");
    assert_eq!((a1, a2), (1, 2));
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.attempts, 2);
    assert_eq!(got.error.as_deref(), Some("registry 503"));
    // State unchanged by failure bookkeeping — the scanner decides
    // when the budget is spent.
    assert_eq!(got.state, EnableJobState::Materializing);

    // Retry on a non-failed job is a Conflict.
    match meta.retry_enable_job(job.id).await {
        Err(MetaError::Conflict(msg)) => assert!(msg.contains("materializing"), "got: {msg}"),
        other => panic!("expected Conflict, got {other:?}"),
    }

    // failed → retry → pending with counters reset.
    meta.set_enable_job_state(job.id, EnableJobState::Failed)
        .await
        .expect("failed");
    // `failed` preserves the error for the operator.
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.error.as_deref(), Some("registry 503"));
    let retried = meta.retry_enable_job(job.id).await.expect("retry");
    assert_eq!(retried.state, EnableJobState::Pending);
    assert_eq!(retried.attempts, 0);
    assert_eq!(retried.error, None);

    // Unknown id → NotFound.
    match meta.retry_enable_job(Uuid::new_v4()).await {
        Err(MetaError::NotFound) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }

    // Cleanup.
    meta.set_enable_job_state(job.id, EnableJobState::Failed)
        .await
        .expect("park");
}

/// ADR 0036 P4: content-keyed base-snapshot reuse lookup. Seeds an
/// enabled image whose `disk_manifest_*` is a (simulated)
/// content-derived ref + a base snapshot, then asserts the lookup
/// finds it by content — including after soft-delete — and misses on
/// a different manifest.toml or different content.
#[tokio::test]
#[ignore]
async fn find_enabled_image_by_content_keys_on_disk_manifest_and_toml() {
    let Some(meta) = connect().await else { return };

    let snapshot_id = SnapshotId::new();
    meta.record_snapshot(SnapshotRecord {
        id: snapshot_id,
        session_id: None,
        host_id: None,
        image_version: "p4-fixture".into(),
        size_bytes: 0,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: None,
        memory_manifest: None,
        recoverable: true,
        aux_bundles: vec![],
    })
    .await
    .expect("seed base snapshot");

    let content_ref = ManifestRef::new(); // stands in for a content-derived ref
    let toml = format!("name = \"p4-{}\"\n", Uuid::new_v4());
    let uri = unique_uri("content-reuse");
    let now = Utc::now();
    meta.upsert_enabled_image(EnabledImage {
        id: Uuid::new_v4(),
        image_uri: uri.clone(),
        manifest_toml: toml.clone(),
        manifest_digest: "sha256:p4-digest".into(),
        disk_manifest: Some(content_ref),
        base_snapshot_id: Some(snapshot_id),
        base_snapshot_disk_manifest: Some(ManifestRef::new()),
        base_snapshot_memory_manifest: None,
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
        soft_deleted_at: None,
    })
    .await
    .expect("seed enabled image");

    // Hit: same content + same toml → the row, regardless of URI.
    let found = meta
        .find_enabled_image_by_content(content_ref, &toml)
        .await
        .expect("lookup")
        .expect("content match must be found");
    assert_eq!(found.image_uri, uri);
    assert_eq!(found.base_snapshot_id, Some(snapshot_id));

    // Miss: same content, different manifest.toml (env change must
    // force a fresh capture).
    assert!(meta
        .find_enabled_image_by_content(content_ref, "name = \"other\"\n")
        .await
        .expect("lookup other toml")
        .is_none());

    // Miss: different content.
    assert!(meta
        .find_enabled_image_by_content(ManifestRef::new(), &toml)
        .await
        .expect("lookup other content")
        .is_none());

    // Still a hit after soft-delete — the snapshot lineage stays
    // pinned and reusable even when the row is disabled.
    meta.soft_delete_enabled_image(&uri)
        .await
        .expect("soft delete");
    let found = meta
        .find_enabled_image_by_content(content_ref, &toml)
        .await
        .expect("lookup post-delete")
        .expect("soft-deleted rows must still match");
    assert_eq!(found.image_uri, uri);
}
