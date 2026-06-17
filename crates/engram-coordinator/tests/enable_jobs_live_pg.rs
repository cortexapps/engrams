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

    // Terminal job → a fresh enable inserts a NEW row. Every state
    // write is now fenced by `claimed_by` (#232), so claim first.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim a");
    meta.set_enable_job_state(a.id, "pod-a", EnableJobState::Ready)
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
    // other tests don't pick it up. pod-b holds the claim from the
    // expired-lease re-claim above.
    meta.set_enable_job_state(job.id, "pod-b", EnableJobState::Failed)
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

    // Claim the job — every post-claim write is fenced by
    // `claimed_by` now (#232), so the holder must own the lease.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim");

    // Progress: total stamped once, done advances.
    meta.update_enable_job_progress(job.id, "pod-a", 0, Some(625))
        .await
        .expect("stamp total");
    meta.update_enable_job_progress(job.id, "pod-a", 100, None)
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
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Materializing)
        .await
        .expect("materializing");
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.state, EnableJobState::Materializing);

    // Failures bump attempts and store the error. A failure RELEASES
    // the claim (claimed_by → NULL), so re-claim before the next one —
    // exactly what the scanner's next tick does. Below budget + not
    // forced → state is unchanged (the flip happens atomically in the
    // SAME call once the budget is spent).
    let (a1, s1) = meta
        .record_enable_job_failure(job.id, "pod-a", "registry 429", 5, false)
        .await
        .expect("fail 1");
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("re-claim");
    let (a2, s2) = meta
        .record_enable_job_failure(job.id, "pod-a", "registry 503", 5, false)
        .await
        .expect("fail 2");
    assert_eq!((a1, a2), (1, 2));
    assert_eq!(
        (s1, s2),
        (EnableJobState::Materializing, EnableJobState::Materializing)
    );
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.attempts, 2);
    assert_eq!(got.error.as_deref(), Some("registry 503"));
    // State unchanged below budget — the atomic flip waits for the budget.
    assert_eq!(got.state, EnableJobState::Materializing);

    // Budget exhausted on the SAME call: attempts+1 (3) >= max_attempts (3)
    // ⇒ atomically flips to `failed`, no follow-up write. This is the
    // runaway-attempts regression: pre-fix, the flip lived in a separate
    // fenced set_enable_job_state that fence-missed (claim already released)
    // and silently no-op'd, so the job never went terminal.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("re-claim for budget");
    let (a3, s3) = meta
        .record_enable_job_failure(job.id, "pod-a", "registry 500", 3, false)
        .await
        .expect("fail 3");
    assert_eq!(a3, 3);
    assert_eq!(
        s3,
        EnableJobState::Failed,
        "budget spent must flip to failed atomically"
    );
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.state, EnableJobState::Failed);
    assert_eq!(got.error.as_deref(), Some("registry 500"));

    // Reset to exercise the non-retryable (force_terminal) path: a single
    // deterministic failure (e.g. a [warm] hook exit) flips to failed on
    // the FIRST occurrence, well under budget.
    let retried = meta
        .retry_enable_job(job.id)
        .await
        .expect("retry to pending");
    assert_eq!(retried.state, EnableJobState::Pending);
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("re-claim for force-terminal");
    let (a_ft, s_ft) = meta
        .record_enable_job_failure(job.id, "pod-a", "[warm] hook exited 1", 5, true)
        .await
        .expect("force terminal");
    assert_eq!(a_ft, 1, "first attempt");
    assert_eq!(
        s_ft,
        EnableJobState::Failed,
        "non-retryable must bail fast on attempt 1"
    );
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.state, EnableJobState::Failed);
    assert_eq!(got.error.as_deref(), Some("[warm] hook exited 1"));

    // Reset back to materializing for the remaining retry-state assertions
    // below (they expect a non-terminal, claimable job).
    let retried = meta
        .retry_enable_job(job.id)
        .await
        .expect("retry to pending 2");
    assert_eq!(retried.state, EnableJobState::Pending);
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("re-claim after reset");
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Materializing)
        .await
        .expect("back to materializing");

    // Retry on a non-failed job is a Conflict.
    match meta.retry_enable_job(job.id).await {
        Err(MetaError::Conflict(msg)) => assert!(msg.contains("materializing"), "got: {msg}"),
        other => panic!("expected Conflict, got {other:?}"),
    }

    // failed → retry → pending with counters reset. The 2nd failure
    // released the claim, so re-claim before flipping state.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("re-claim for failed");
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Failed)
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

    // Cleanup. Retry reset the claim, so re-claim before parking.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("re-claim for park");
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Failed)
        .await
        .expect("park");
}

/// Issue #232 regression: enable-job lease fencing token.
///
/// `claim_enable_jobs` arbitrates who STARTS a job, but before this
/// fix every post-claim write (`update_enable_job_progress`,
/// `set_enable_job_state`, `record_enable_job_failure`) was `WHERE id
/// = $1` only — so an expired-lease pod could stomp the new
/// claimant: reset `chunks_done`, renew `claimed_at` on the wrong
/// pod's behalf, flip `state`, and clear/steal the claim.
///
/// This test claims as pod-a, expires the lease, re-claims as pod-b,
/// then proves every one of pod-a's stale writes returns `Conflict`
/// and mutates NOTHING (state, chunks_done, attempts, claimed_by,
/// error all unchanged), while pod-b's identical writes succeed.
#[tokio::test]
#[ignore]
async fn stale_claimant_writes_are_fenced_off() {
    let Some(meta) = connect().await else { return };
    let uri = unique_uri("fencing");
    let job = meta
        .create_or_get_enable_job(&uri, None)
        .await
        .expect("create");

    // pod-a claims, then drives the job partway: stamps total + 50
    // chunks done and advances to materializing.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim a");
    meta.update_enable_job_progress(job.id, "pod-a", 50, Some(625))
        .await
        .expect("a progress");
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Materializing)
        .await
        .expect("a materializing");

    // pod-a's lease expires (lease_secs = 0 → instantly stale) and
    // pod-b legitimately re-claims. The crashed/slow-pod recovery
    // path: one atomic claim hands ownership to pod-b.
    let reclaimed = meta
        .claim_enable_jobs("pod-b", 0, 50)
        .await
        .expect("claim b");
    assert!(
        reclaimed.iter().any(|j| j.id == job.id),
        "pod-b must re-claim the expired-lease job"
    );

    // Snapshot the row as pod-b sees it right after re-claiming.
    let before = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(before.chunks_done, 50);
    assert_eq!(before.attempts, 0);
    assert_eq!(before.state, EnableJobState::Materializing);

    // --- pod-a is now stale. Every write it attempts must Conflict. ---

    // 1. A stale progress checkpoint must NOT reset chunks_done to
    //    pod-a's number, and must NOT renew the lease on pod-a's
    //    behalf (the core bug — a renewing stale tick makes pod-b's
    //    claim look perpetually contested).
    match meta
        .update_enable_job_progress(job.id, "pod-a", 7, None)
        .await
    {
        Err(MetaError::Conflict(msg)) => {
            assert!(msg.contains("pod-b"), "should name the new holder: {msg}")
        }
        other => panic!("stale progress write must Conflict, got {other:?}"),
    }

    // 2. A stale state flip must not drive a job pod-b owns backward.
    match meta
        .set_enable_job_state(job.id, "pod-a", EnableJobState::Capturing)
        .await
    {
        Err(MetaError::Conflict(_)) => {}
        other => panic!("stale state write must Conflict, got {other:?}"),
    }

    // 3. A stale failure must not bump attempts, stamp error, or clear
    //    pod-b's claim out from under it.
    match meta
        .record_enable_job_failure(job.id, "pod-a", "stale transient error", 5, false)
        .await
    {
        Err(MetaError::Conflict(_)) => {}
        other => panic!("stale failure write must Conflict, got {other:?}"),
    }

    // The row is byte-for-byte unchanged by all three stale writes.
    let after = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(after.chunks_done, before.chunks_done, "chunks_done stomped");
    assert_eq!(
        after.chunks_total, before.chunks_total,
        "chunks_total stomped"
    );
    assert_eq!(
        after.attempts, before.attempts,
        "attempts bumped by stale pod"
    );
    assert_eq!(after.state, before.state, "state flipped by stale pod");
    assert_eq!(after.error, before.error, "error stamped by stale pod");

    // pod-b — the rightful holder — can still drive the job.
    meta.update_enable_job_progress(job.id, "pod-b", 600, None)
        .await
        .expect("b progress");
    meta.set_enable_job_state(job.id, "pod-b", EnableJobState::Capturing)
        .await
        .expect("b capturing");
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.chunks_done, 600);
    assert_eq!(got.state, EnableJobState::Capturing);

    // A write fenced against a genuinely absent row is NotFound, not
    // Conflict — keeps the disambiguation honest.
    match meta
        .update_enable_job_progress(Uuid::new_v4(), "pod-b", 1, None)
        .await
    {
        Err(MetaError::NotFound) => {}
        other => panic!("unknown id must be NotFound, got {other:?}"),
    }

    // Cleanup: pod-b parks it terminal.
    meta.set_enable_job_state(job.id, "pod-b", EnableJobState::Failed)
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
        events_cursor: None,
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
