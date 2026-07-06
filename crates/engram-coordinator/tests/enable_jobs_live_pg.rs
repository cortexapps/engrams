//! Live-Postgres tests for the ADR 0036 `enable_jobs` MetadataStore
//! surface: create-or-get dedup (one active job per URI), lease-based
//! claiming (multi-coordinator safety + expiry steal), progress
//! checkpoints, failure bookkeeping, the admin retry transition, and
//! (issue #538) the `prestaging`-stage store surface
//! (`begin_enable_job_prestage` / `set_enable_job_prestage_hosts` /
//! `list_prestaging_refs`). The scanner's ORCHESTRATION of that surface
//! (the poll loop, the deadline policy, the `eval_prestage` truth table) is
//! pure and unit-tested in `enable_scanner.rs` — driving the full pipeline
//! here would need a fake OCI registry + capture host, which is the FC e2e
//! suite's job (see this file's original scope note above); what's new and
//! testable against REAL Postgres is the fenced store surface itself.
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
use engram_core::types::host::{
    HostCapacity, HostHeartbeat, HostRecord, HostStatus, HostUtilization,
};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{EnableJobState, EnabledImage, HostId};
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
        .create_or_get_enable_job(&uri, Some("sha256:digest-a"), &[])
        .await
        .expect("create");
    assert_eq!(a.state, EnableJobState::Pending);
    assert_eq!(a.image_uri, uri);
    assert_eq!(a.chunks_done, 0);

    // Re-POST while in flight → same job, even with a moved digest.
    let b = meta
        .create_or_get_enable_job(&uri, Some("sha256:digest-b"), &[])
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
        .create_or_get_enable_job(&uri, None, &[])
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
        .create_or_get_enable_job(&uri, None, &[])
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
        .create_or_get_enable_job(&uri, None, &[])
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
    // (Budget-exhaustion + force-terminal flips are covered with fresh jobs
    // in `failure_flips_failed_atomically_at_budget_and_on_force_terminal`,
    // so this test's downstream retry-state assertions run on a job whose
    // error/state history isn't perturbed.)
    assert_eq!(got.state, EnableJobState::Materializing);

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

/// 1a + 1b regression: a failure flips to `failed` ATOMICALLY — in the SAME
/// fenced write that bumps `attempts` and releases the claim — when the budget
/// is spent (`attempts + 1 >= max_attempts`) or the failure is non-retryable
/// (`force_terminal`).
///
/// Pre-fix the flip lived in a SEPARATE `set_enable_job_state(Failed)` that ran
/// after `record_enable_job_failure` had already nulled `claimed_by`; that write
/// fence-missed and silently no-op'd, so the job never went terminal — it was
/// re-claimed and re-failed every tick, blowing past `max_attempts` (the 550+
/// runaway that bricked the fleet).
#[tokio::test]
#[ignore]
async fn failure_flips_failed_atomically_at_budget_and_on_force_terminal() {
    let Some(meta) = connect().await else { return };

    // --- Budget path (1a): max_attempts = 2. ---
    let uri = unique_uri("budget");
    let job = meta
        .create_or_get_enable_job(&uri, None, &[])
        .await
        .expect("create");
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim");
    let (a1, s1) = meta
        .record_enable_job_failure(job.id, "pod-a", "transient 1", 2, false)
        .await
        .expect("fail 1");
    assert_eq!(a1, 1);
    assert_ne!(
        s1,
        EnableJobState::Failed,
        "first of 2 attempts must stay non-terminal"
    );
    // The failure released the claim — re-claim like the scanner's next tick.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("re-claim");
    let (a2, s2) = meta
        .record_enable_job_failure(job.id, "pod-a", "transient 2", 2, false)
        .await
        .expect("fail 2");
    assert_eq!(a2, 2);
    assert_eq!(
        s2,
        EnableJobState::Failed,
        "budget spent must flip to failed in the SAME call"
    );
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.state, EnableJobState::Failed);
    assert_eq!(got.error.as_deref(), Some("transient 2"));

    // --- Force-terminal path (1b): a deterministic failure (e.g. a [warm]
    //     hook non-zero exit) bails on attempt 1, well under a generous budget. ---
    let uri2 = unique_uri("force-terminal");
    let job2 = meta
        .create_or_get_enable_job(&uri2, None, &[])
        .await
        .expect("create 2");
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim 2");
    let (a, s) = meta
        .record_enable_job_failure(job2.id, "pod-a", "[warm] hook exited 1", 5, true)
        .await
        .expect("force terminal");
    assert_eq!(a, 1, "first attempt");
    assert_eq!(
        s,
        EnableJobState::Failed,
        "non-retryable must bail fast on attempt 1, not burn the budget"
    );
    let got2 = meta.get_enable_job(job2.id).await.unwrap().unwrap();
    assert_eq!(got2.state, EnableJobState::Failed);
    assert_eq!(got2.error.as_deref(), Some("[warm] hook exited 1"));
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
        .create_or_get_enable_job(&uri, None, &[])
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

/// Issue #539: `update_enable_job_capture_progress` is claim-fenced
/// exactly like `update_enable_job_progress` (a peer claimant's write
/// must `Conflict`, not stomp the row), it renews the lease
/// (`claimed_at`), and `record_enable_job_failure` — which never
/// touches `warm_stage`/`output_tail` itself — leaves whatever the last
/// progress write stamped in place. This is the acceptance-criterion
/// path: even a `WarmExecTransport` kill (stream dies mid-run, no
/// further progress write possible) must leave the failing stage + tail
/// on the row from the last successful write before the kill.
#[tokio::test]
#[ignore]
async fn capture_progress_is_fenced_renews_lease_and_survives_failure() {
    use engram_core::types::{
        CaptureFailureKind, CapturePhase, CaptureProgress, WarmStageOutcome, WarmStageRecord,
    };

    let Some(meta) = connect().await else { return };
    let uri = unique_uri("capture-progress");
    let job = meta
        .create_or_get_enable_job(&uri, None, &[])
        .await
        .expect("create");
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim a");

    let stage_started = Utc::now();
    let progress = CaptureProgress {
        phase: CapturePhase::Warm,
        warm_stage: Some("uiresources-wait".into()),
        detail: Some("waiting on uiresources/brain-backend".into()),
        output_tail: "error: timed out waiting for the condition on uiresources/brain-backend"
            .into(),
        warm_stages: vec![WarmStageRecord {
            name: "uiresources-wait".into(),
            started_at: stage_started,
            ended_at: None,
            outcome: WarmStageOutcome::Running,
        }],
    };
    meta.update_enable_job_capture_progress(job.id, "pod-a", &progress)
        .await
        .expect("pod-a progress write");

    let after_progress = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(after_progress.capture_phase, Some(CapturePhase::Warm));
    assert_eq!(
        after_progress.warm_stage.as_deref(),
        Some("uiresources-wait")
    );
    assert_eq!(
        after_progress.warm_stage_started_at.map(|t| t.timestamp()),
        Some(stage_started.timestamp())
    );
    assert_eq!(after_progress.warm_stages.len(), 1);
    assert!(after_progress
        .output_tail
        .as_deref()
        .unwrap()
        .contains("uiresources/brain-backend"));

    // The progress write renewed the lease: an immediate re-claim
    // attempt at lease_secs=300 must NOT hand the job to a peer (it's
    // not expired).
    let stolen = meta.claim_enable_jobs("pod-b", 300, 50).await.unwrap();
    assert!(
        !stolen.iter().any(|j| j.id == job.id),
        "a fresh progress write must have renewed the lease — pod-b must not re-claim"
    );

    // A peer's write against a claim it doesn't hold is fenced off,
    // exactly like update_enable_job_progress.
    let peer_progress = CaptureProgress {
        phase: CapturePhase::Warm,
        warm_stage: Some("peer-stage".into()),
        detail: None,
        output_tail: "peer output".into(),
        warm_stages: vec![],
    };
    match meta
        .update_enable_job_capture_progress(job.id, "pod-b", &peer_progress)
        .await
    {
        Err(MetaError::Conflict(msg)) => assert!(msg.contains("pod-a"), "{msg}"),
        other => panic!("stale-claimant capture progress write must Conflict, got {other:?}"),
    }
    let unchanged = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(
        unchanged.warm_stage.as_deref(),
        Some("uiresources-wait"),
        "a fenced-off peer write must not stomp the row"
    );

    // record_enable_job_failure never touches warm_stage/output_tail —
    // they must survive the failure exactly as the last progress write
    // left them (the diagnosis a `status None` / WarmExecTransport kill
    // used to lose entirely).
    meta.record_enable_job_failure(
        job.id,
        "pod-a",
        &format!(
            "base-snapshot capture failed ({}): in-guest wait timed out",
            CaptureFailureKind::WarmStageDeadline
        ),
        5,
        true,
    )
    .await
    .expect("record failure");

    let failed = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(failed.state, EnableJobState::Failed);
    assert!(failed.error.unwrap().contains("in-guest wait timed out"));
    assert_eq!(
        failed.warm_stage.as_deref(),
        Some("uiresources-wait"),
        "failing stage must survive record_enable_job_failure"
    );
    assert!(
        failed
            .output_tail
            .as_deref()
            .unwrap()
            .contains("uiresources/brain-backend"),
        "output tail must survive record_enable_job_failure"
    );
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
        fc_snapshot_version: None,
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
        capture_env: Vec::new(),
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

// ---- ADR 0036 amendment (issue #538): the `prestaging`-stage store surface ----

/// A schedulable, staging-eligible host reporting `digest` in
/// `ready_images` iff `staged`.
async fn seed_staging_host(meta: &Arc<dyn MetadataStore>, digest: &str, staged: bool) -> HostId {
    let id = HostId::new();
    meta.upsert_host(HostRecord {
        id,
        hostname: format!("prestage-{id}"),
        cloud_metadata: Default::default(),
        capacity: HostCapacity {
            total_gb: 0,
            used_gb: 0,
            total_mib: 0,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: HostUtilization::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: None,
        ready_images: Vec::new(),
        local_snapshots: Vec::new(),
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: true,
        capabilities: Default::default(),
    })
    .await
    .expect("upsert staging host");
    meta.touch_host_heartbeat(
        id,
        HostHeartbeat {
            status: HostStatus::Ready,
            capacity: HostCapacity {
                total_gb: 0,
                used_gb: 0,
                total_mib: 0,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: HostUtilization::default(),
            ready_images: if staged {
                vec![digest.to_string()]
            } else {
                Vec::new()
            },
            local_snapshots: Vec::new(),
            current_bundles: Vec::new(),
            total_vcpus: 0,
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: true,
            capabilities: Default::default(),
        },
    )
    .await
    .expect("heartbeat staging host");
    id
}

fn prestage_ref_json(digest: &str) -> serde_json::Value {
    serde_json::json!({
        "image_uri": "localhost:5001/prestage-test:warm",
        "manifest_digest": digest,
        "base_snapshot_id": SnapshotId::new().to_string(),
        "base_snapshot_disk_manifest": { "manifest_id": Uuid::new_v4().to_string(), "version": 1 },
        "base_snapshot_memory_manifest": null,
    })
}

/// (a) `begin_enable_job_prestage` flips `capturing → prestaging` and
/// stamps the ref in ONE fenced write; the job round-trips (state +
/// `prestage_hosts` default `{}`); `list_prestaging_refs` surfaces exactly
/// the jobs currently `prestaging` (and none of the others) — the read
/// the heartbeat-ack handler drives `prestage_images` from.
#[tokio::test]
#[ignore]
async fn begin_prestage_transitions_state_and_ref_is_listed() {
    let Some(meta) = connect().await else { return };
    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    let uri = unique_uri("prestage-transition");
    let job = meta
        .create_or_get_enable_job(&uri, Some(&digest), &[])
        .await
        .expect("create");
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim");
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Capturing)
        .await
        .expect("capturing");

    // A DIFFERENT job stays `pending` — it must never show up in
    // `list_prestaging_refs`, proving the read filters on state, not just
    // ref-presence.
    let other_uri = unique_uri("prestage-transition-control");
    meta.create_or_get_enable_job(&other_uri, None, &[])
        .await
        .expect("create control job");

    let the_ref = prestage_ref_json(&digest);
    meta.begin_enable_job_prestage(job.id, "pod-a", the_ref.clone())
        .await
        .expect("begin prestage");

    let got = meta
        .get_enable_job(job.id)
        .await
        .unwrap()
        .expect("job exists");
    assert_eq!(got.state, EnableJobState::Prestaging);
    assert_eq!(got.prestage_hosts, serde_json::json!({}));

    let refs = meta.list_prestaging_refs().await.expect("list refs");
    assert_eq!(refs.len(), 1, "only the prestaging job's ref is listed");
    assert_eq!(refs[0]["manifest_digest"], digest);

    // Advance past prestaging — the ref must drop out of the list (it's
    // scoped to jobs ACTIVELY prestaging, not a durable advertisement).
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Ready)
        .await
        .expect("ready");
    let refs_after = meta.list_prestaging_refs().await.expect("list refs after");
    assert!(
        refs_after.is_empty(),
        "a ready job's ref must no longer be advertised"
    );

    // Cleanup: park the control job terminal.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim control");
    for j in meta.list_enable_jobs(50).await.unwrap() {
        if j.image_uri == other_uri {
            meta.set_enable_job_state(j.id, "pod-a", EnableJobState::Failed)
                .await
                .expect("park control");
        }
    }
}

/// (b)/(c) `set_enable_job_prestage_hosts` records the per-host outcome
/// map the scanner computes from a hosts snapshot — one staged, one
/// straggler `timed_out`, matching the deadline-with-≥1-staged policy
/// (proceed to ready with stragglers recorded, never wedge). Also proves
/// the write is a plain audit record: it doesn't itself flip job state.
#[tokio::test]
#[ignore]
async fn set_prestage_hosts_records_the_outcome_map() {
    let Some(meta) = connect().await else { return };
    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    let staged_host = seed_staging_host(&meta, &digest, true).await;
    let straggler_host = seed_staging_host(&meta, &digest, false).await;

    let uri = unique_uri("prestage-outcomes");
    let job = meta
        .create_or_get_enable_job(&uri, Some(&digest), &[])
        .await
        .expect("create");
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim");
    meta.begin_enable_job_prestage(job.id, "pod-a", prestage_ref_json(&digest))
        .await
        .expect("begin prestage");

    let outcomes = serde_json::json!({
        staged_host.to_string(): { "outcome": "staged", "waited_ms": 4200 },
        straggler_host.to_string(): { "outcome": "timed_out", "waited_ms": 1_200_000 },
    });
    meta.set_enable_job_prestage_hosts(job.id, "pod-a", outcomes.clone())
        .await
        .expect("set outcomes");

    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.prestage_hosts, outcomes);
    // The deadline-with-stragglers policy proceeds to ready — this call
    // alone must not have flipped state.
    assert_eq!(got.state, EnableJobState::Prestaging);

    // The scanner's next write in the real pipeline is the Ready flip;
    // exercise it here to confirm the audit column survives untouched.
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Ready)
        .await
        .expect("ready");
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(
        got.prestage_hosts, outcomes,
        "the ready flip must not clobber the audit map"
    );
}

/// (d) #232 fencing: a peer's re-claim mid-prestage must make the stale
/// pod's `begin_enable_job_prestage` / `set_enable_job_prestage_hosts`
/// Conflict — the SAME discipline every other enable-job write already
/// has (`stale_claimant_writes_are_fenced_off` above), extended to the two
/// new prestage methods.
#[tokio::test]
#[ignore]
async fn prestage_writes_are_fenced_off_from_a_stale_claimant() {
    let Some(meta) = connect().await else { return };
    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    let uri = unique_uri("prestage-fencing");
    let job = meta
        .create_or_get_enable_job(&uri, Some(&digest), &[])
        .await
        .expect("create");

    // pod-a claims and drives to capturing.
    meta.claim_enable_jobs("pod-a", 300, 50)
        .await
        .expect("claim a");
    meta.set_enable_job_state(job.id, "pod-a", EnableJobState::Capturing)
        .await
        .expect("a capturing");

    // pod-a's lease expires; pod-b legitimately re-claims and begins
    // prestage.
    meta.claim_enable_jobs("pod-b", 0, 50)
        .await
        .expect("claim b");
    meta.begin_enable_job_prestage(job.id, "pod-b", prestage_ref_json(&digest))
        .await
        .expect("b begin prestage");
    let before = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(before.state, EnableJobState::Prestaging);

    // pod-a is now stale — both new methods must Conflict and mutate
    // nothing.
    match meta
        .begin_enable_job_prestage(job.id, "pod-a", prestage_ref_json("sha256:stale-attempt"))
        .await
    {
        Err(MetaError::Conflict(msg)) => assert!(msg.contains("pod-b"), "got: {msg}"),
        other => panic!("stale begin_enable_job_prestage must Conflict, got {other:?}"),
    }
    match meta
        .set_enable_job_prestage_hosts(job.id, "pod-a", serde_json::json!({"x": "y"}))
        .await
    {
        Err(MetaError::Conflict(_)) => {}
        other => panic!("stale set_enable_job_prestage_hosts must Conflict, got {other:?}"),
    }
    let after = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(after.state, before.state, "stale pod flipped state");
    assert_eq!(
        after.prestage_hosts, before.prestage_hosts,
        "stale pod stomped the audit map"
    );

    // pod-b — the rightful holder — can still drive the job.
    meta.set_enable_job_prestage_hosts(job.id, "pod-b", serde_json::json!({"real": "outcome"}))
        .await
        .expect("b set outcomes");
    meta.set_enable_job_state(job.id, "pod-b", EnableJobState::Ready)
        .await
        .expect("b ready");
    let got = meta.get_enable_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.state, EnableJobState::Ready);
}
