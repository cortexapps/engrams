//! Live-Postgres tests for the ADR 0081 `capture_jobs`/`cold_bases`
//! `MetadataStore` surface: insert-or-get dedup (one active job per
//! enable job), the epoch-fenced report write (stale-epoch rejection,
//! terminal Done/Failed landing, post-terminal immutability),
//! reassignment (epoch/attempts bump + stage reset), the per-stage
//! deadline scan, the heartbeat-ack-adjacent bulk reads
//! (`capture_assignments_for_host`/`hosts_with_live_capture_jobs`), and
//! the `cold_bases` reuse-lookup round trip.
//!
//! This is P1a (dormant): nothing in production calls these verbs yet
//! (the executor/scanner rework lands in a later commit) — this suite
//! exercises the fenced store surface itself, exactly like
//! `enable_jobs_live_pg.rs` does for the sibling `enable_jobs` verbs.
//!
//! `#[ignore]`'d by default; requires Postgres reachable at
//! `ENGRAM_TEST_DATABASE_URL`. Run:
//!
//! ```bash
//! docker compose -f deploy/docker-compose.dev.yml up -d postgres
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test capture_jobs_live_pg -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::{
    CaptureJobProgress, CaptureJobReport, CaptureJobStage, CaptureTerminalReport, ColdBaseRow,
    NewCaptureJob,
};
use engram_core::{HostId, SnapshotId};
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
    format!("test-registry.local/capture-jobs/{tag}:{}", Uuid::new_v4())
}

/// Minimal valid ImageConfig for job fixtures (mirrors
/// `enable_jobs_live_pg.rs`'s helper of the same shape).
fn test_config() -> engram_core::types::image::ImageConfig {
    toml::from_str("name = \"capture-jobs-fixture\"\n[resources]\nsuggested_vcpus = 2\n").unwrap()
}

/// Seed a fresh `enable_jobs` row (the FK `capture_jobs.enable_job_id`
/// requires) and return its id.
async fn seed_enable_job(meta: &Arc<dyn MetadataStore>, tag: &str) -> Uuid {
    let uri = unique_uri(tag);
    meta.create_or_get_enable_job(&uri, None, &test_config())
        .await
        .expect("seed enable job")
        .id
}

fn new_capture_job(enable_job_id: Uuid, host_id: HostId) -> NewCaptureJob {
    NewCaptureJob {
        enable_job_id,
        image_uri: format!("test-registry.local/capture-jobs/img/{}", Uuid::new_v4()),
        manifest_digest: "sha256:deadbeef".to_string(),
        disk_manifest: format!("{}@v1", Uuid::new_v4()),
        image_config: test_config(),
        oci_defaults: Default::default(),
        host_id,
    }
}

#[tokio::test]
#[ignore]
async fn insert_capture_job_dedups_active_jobs_per_enable_job() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "dedup").await;
    let host = HostId::new();

    let a = meta
        .insert_capture_job(new_capture_job(enable_job_id, host))
        .await
        .expect("insert a");
    assert_eq!(a.stage, CaptureJobStage::Assigned);
    assert_eq!(a.epoch, 1);
    assert_eq!(a.attempts, 1);
    assert_eq!(a.enable_job_id, enable_job_id);

    // Re-insert while the job is still active (e.g. a re-driven scanner
    // tick) → same job, never duplicated.
    let b = meta
        .insert_capture_job(new_capture_job(enable_job_id, host))
        .await
        .expect("insert b");
    assert_eq!(b.id, a.id, "active job must be returned, not duplicated");

    // Terminal it, then a fresh insert makes a NEW row — a terminal job
    // must never block a subsequent recapture.
    let report = CaptureJobReport {
        job_id: a.id,
        epoch: a.epoch,
        stage: CaptureJobStage::Done,
        progress: None,
        fc_snapshot_version: Some("v6".into()),
        terminal: Some(CaptureTerminalReport::Done {
            result_bincode: vec![1, 2, 3],
        }),
    };
    assert!(meta
        .record_capture_job_report(&report)
        .await
        .expect("terminal report"));

    let c = meta
        .insert_capture_job(new_capture_job(enable_job_id, host))
        .await
        .expect("insert c after terminal");
    assert_ne!(c.id, a.id, "a terminal job must not block a fresh capture");
}

#[tokio::test]
#[ignore]
async fn record_capture_job_report_is_fenced_by_epoch() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "fenced").await;
    let host = HostId::new();
    let job = meta
        .insert_capture_job(new_capture_job(enable_job_id, host))
        .await
        .expect("insert");

    // Wrong epoch (the job is fresh, at epoch 1 — this claims epoch 2,
    // as if a reassignment had happened that never actually landed).
    let stale_report = CaptureJobReport {
        job_id: job.id,
        epoch: job.epoch + 1,
        stage: CaptureJobStage::Booting,
        progress: Some(CaptureJobProgress {
            detail: Some("should never land".into()),
            log_tail: None,
        }),
        fc_snapshot_version: None,
        terminal: None,
    };
    let updated = meta
        .record_capture_job_report(&stale_report)
        .await
        .expect("stale report call");
    assert!(!updated, "a mismatched epoch must not update the row");

    let got = meta.get_capture_job(job.id).await.unwrap().unwrap();
    assert_eq!(
        got.stage,
        CaptureJobStage::Assigned,
        "stage must be untouched by the fenced-off write"
    );
    assert!(got.stage_progress.is_none());

    // The correct epoch DOES land.
    let good_report = CaptureJobReport {
        job_id: job.id,
        epoch: job.epoch,
        stage: CaptureJobStage::Booting,
        progress: Some(CaptureJobProgress {
            detail: Some("cold boot".into()),
            log_tail: None,
        }),
        fc_snapshot_version: None,
        terminal: None,
    };
    assert!(meta
        .record_capture_job_report(&good_report)
        .await
        .expect("good report"));
    let got = meta.get_capture_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.stage, CaptureJobStage::Booting);
    assert_eq!(
        got.stage_progress.unwrap().detail.as_deref(),
        Some("cold boot")
    );
}

#[tokio::test]
#[ignore]
async fn terminal_reports_land_once_and_become_immutable() {
    let Some(meta) = connect().await else { return };
    let host = HostId::new();

    // --- Done path ---
    let enable_job_done = seed_enable_job(&meta, "terminal-done").await;
    let job = meta
        .insert_capture_job(new_capture_job(enable_job_done, host))
        .await
        .expect("insert done job");
    let done_report = CaptureJobReport {
        job_id: job.id,
        epoch: job.epoch,
        stage: CaptureJobStage::Done,
        progress: None,
        fc_snapshot_version: Some("v9".into()),
        terminal: Some(CaptureTerminalReport::Done {
            result_bincode: vec![9, 9, 9],
        }),
    };
    assert!(meta
        .record_capture_job_report(&done_report)
        .await
        .expect("done report"));
    let got = meta.get_capture_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.stage, CaptureJobStage::Done);
    assert_eq!(got.result_bincode.as_deref(), Some(&[9, 9, 9][..]));
    assert_eq!(got.fc_snapshot_version.as_deref(), Some("v9"));

    // Re-advertising the SAME terminal report (the host keeps sending it
    // until acked) must be a no-op: the row is already terminal.
    let retried = meta
        .record_capture_job_report(&done_report)
        .await
        .expect("retry done report");
    assert!(!retried, "an already-done job must reject a further report");

    // --- Failed path ---
    let enable_job_failed = seed_enable_job(&meta, "terminal-failed").await;
    let job2 = meta
        .insert_capture_job(new_capture_job(enable_job_failed, host))
        .await
        .expect("insert failed job");
    let failed_report = CaptureJobReport {
        job_id: job2.id,
        epoch: job2.epoch,
        stage: CaptureJobStage::Failed,
        progress: None,
        fc_snapshot_version: None,
        terminal: Some(CaptureTerminalReport::Failed {
            error: "warm hook exited 1".into(),
            error_stage: "warming".into(),
            retryable: false,
        }),
    };
    assert!(meta
        .record_capture_job_report(&failed_report)
        .await
        .expect("failed report"));
    let got2 = meta.get_capture_job(job2.id).await.unwrap().unwrap();
    assert_eq!(got2.stage, CaptureJobStage::Failed);
    assert_eq!(got2.error.as_deref(), Some("warm hook exited 1"));
    assert_eq!(got2.error_stage.as_deref(), Some("warming"));
    assert_eq!(got2.retryable, Some(false));

    let retried2 = meta
        .record_capture_job_report(&failed_report)
        .await
        .expect("retry failed report");
    assert!(
        !retried2,
        "an already-failed job must reject a further report"
    );
}

#[tokio::test]
#[ignore]
async fn reassign_bumps_epoch_and_attempts_and_resets_stage() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "reassign").await;
    let host_a = HostId::new();
    let host_b = HostId::new();
    let job = meta
        .insert_capture_job(new_capture_job(enable_job_id, host_a))
        .await
        .expect("insert");

    // Advance it partway so the reassign's stage/progress reset is
    // actually observable (not vacuously true from a fresh row).
    let progress_report = CaptureJobReport {
        job_id: job.id,
        epoch: job.epoch,
        stage: CaptureJobStage::Booting,
        progress: Some(CaptureJobProgress {
            detail: Some("booting".into()),
            log_tail: None,
        }),
        fc_snapshot_version: None,
        terminal: None,
    };
    assert!(meta
        .record_capture_job_report(&progress_report)
        .await
        .expect("progress"));

    let reassigned = meta
        .reassign_capture_job(job.id, job.epoch, host_b)
        .await
        .expect("reassign call")
        .expect("reassign lands");
    assert_eq!(reassigned.epoch, job.epoch + 1);
    assert_eq!(reassigned.attempts, job.attempts + 1);
    assert_eq!(reassigned.host_id, host_b);
    assert_eq!(reassigned.stage, CaptureJobStage::Assigned);
    assert!(
        reassigned.stage_progress.is_none(),
        "reassignment must clear stale stage progress"
    );

    // A reassign against the now-STALE original epoch must miss.
    let stale = meta
        .reassign_capture_job(job.id, job.epoch, host_a)
        .await
        .expect("stale reassign call");
    assert!(stale.is_none(), "a stale-epoch reassign must not land");
}

#[tokio::test]
#[ignore]
async fn expire_capture_job_stages_picks_up_only_over_budget_rows() {
    let Some(meta) = connect().await else { return };
    let host = HostId::new();
    let enable_job_assigned = seed_enable_job(&meta, "expire-assigned").await;
    let enable_job_booting = seed_enable_job(&meta, "expire-booting").await;

    let assigned_job = meta
        .insert_capture_job(new_capture_job(enable_job_assigned, host))
        .await
        .expect("insert assigned");
    let booting_job = meta
        .insert_capture_job(new_capture_job(enable_job_booting, host))
        .await
        .expect("insert booting");
    // Move the second job to `booting` so it's a DIFFERENT stage than
    // the first (which stays `assigned`).
    let advance = CaptureJobReport {
        job_id: booting_job.id,
        epoch: booting_job.epoch,
        stage: CaptureJobStage::Booting,
        progress: None,
        fc_snapshot_version: None,
        terminal: None,
    };
    assert!(meta
        .record_capture_job_report(&advance)
        .await
        .expect("advance to booting"));

    // Immediately: a generous `assigned` budget — neither job is old
    // enough yet, and `booting` isn't in the budget list at all.
    let generous = [(CaptureJobStage::Assigned, Duration::from_secs(3600))];
    let expired_immediately = meta
        .expire_capture_job_stages(&generous)
        .await
        .expect("scan 1");
    assert!(!expired_immediately.iter().any(|j| j.id == assigned_job.id));
    assert!(!expired_immediately.iter().any(|j| j.id == booting_job.id));

    tokio::time::sleep(Duration::from_millis(50)).await;

    // A tiny `assigned` budget: the assigned job is now over budget; the
    // booting job is never selected, no matter its age, because its
    // stage isn't present in the budget list.
    let tiny = [(CaptureJobStage::Assigned, Duration::from_millis(1))];
    let expired = meta.expire_capture_job_stages(&tiny).await.expect("scan 2");
    assert!(
        expired.iter().any(|j| j.id == assigned_job.id),
        "the over-budget assigned job must be listed"
    );
    assert!(
        !expired.iter().any(|j| j.id == booting_job.id),
        "a stage absent from the budget list must never expire"
    );
}

#[tokio::test]
#[ignore]
async fn capture_assignments_for_host_lists_only_active_rows_for_that_host() {
    let Some(meta) = connect().await else { return };
    let host = HostId::new();
    let other_host = HostId::new();

    let enable_job_active = seed_enable_job(&meta, "assignments-active").await;
    let enable_job_done = seed_enable_job(&meta, "assignments-done").await;
    let enable_job_other = seed_enable_job(&meta, "assignments-other-host").await;

    let active = meta
        .insert_capture_job(new_capture_job(enable_job_active, host))
        .await
        .expect("insert active");
    let done = meta
        .insert_capture_job(new_capture_job(enable_job_done, host))
        .await
        .expect("insert done");
    let other = meta
        .insert_capture_job(new_capture_job(enable_job_other, other_host))
        .await
        .expect("insert other-host");

    let done_report = CaptureJobReport {
        job_id: done.id,
        epoch: done.epoch,
        stage: CaptureJobStage::Done,
        progress: None,
        fc_snapshot_version: None,
        terminal: Some(CaptureTerminalReport::Done {
            result_bincode: vec![],
        }),
    };
    assert!(meta
        .record_capture_job_report(&done_report)
        .await
        .expect("terminal done"));

    let assignments = meta
        .capture_assignments_for_host(host)
        .await
        .expect("assignments");
    assert!(
        assignments
            .iter()
            .any(|a| a.job_id == active.id && a.epoch == active.epoch),
        "the active job on this host must be advertised"
    );
    assert!(
        !assignments.iter().any(|a| a.job_id == done.id),
        "a terminal job must not be advertised"
    );
    assert!(
        !assignments.iter().any(|a| a.job_id == other.id),
        "another host's job must not be advertised"
    );
}

#[tokio::test]
#[ignore]
async fn hosts_with_live_capture_jobs_reports_active_hosts_only() {
    let Some(meta) = connect().await else { return };
    let live_host = HostId::new();
    let done_host = HostId::new();

    let enable_job_live = seed_enable_job(&meta, "live-host").await;
    let enable_job_done = seed_enable_job(&meta, "done-host").await;

    meta.insert_capture_job(new_capture_job(enable_job_live, live_host))
        .await
        .expect("insert live");
    let done_job = meta
        .insert_capture_job(new_capture_job(enable_job_done, done_host))
        .await
        .expect("insert done");
    let done_report = CaptureJobReport {
        job_id: done_job.id,
        epoch: done_job.epoch,
        stage: CaptureJobStage::Failed,
        progress: None,
        fc_snapshot_version: None,
        terminal: Some(CaptureTerminalReport::Failed {
            error: "x".into(),
            error_stage: "assigned".into(),
            retryable: false,
        }),
    };
    assert!(meta
        .record_capture_job_report(&done_report)
        .await
        .expect("terminal failed"));

    let live_hosts = meta
        .hosts_with_live_capture_jobs()
        .await
        .expect("live hosts");
    assert!(live_hosts.contains(&live_host));
    assert!(!live_hosts.contains(&done_host));
}

#[tokio::test]
#[ignore]
async fn cold_base_upsert_and_get_round_trip() {
    let Some(meta) = connect().await else { return };
    let content_key = format!("test-content-key-{}", Uuid::new_v4());
    let snapshot_id = SnapshotId::new();

    assert!(
        meta.get_cold_base(&content_key)
            .await
            .expect("miss lookup")
            .is_none(),
        "an unknown content key must miss"
    );

    let row = ColdBaseRow {
        content_key: content_key.clone(),
        snapshot_id,
        disk_manifest: format!("{}@v1", Uuid::new_v4()),
        memory_manifest: format!("{}@v1", Uuid::new_v4()),
        fc_snapshot_version: "v6".into(),
        captured_at: Utc::now(),
        snapshot_bincode: vec![1, 2, 3, 4],
    };
    meta.upsert_cold_base(row.clone()).await.expect("upsert");

    let got = meta
        .get_cold_base(&content_key)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.snapshot_id, snapshot_id);
    assert_eq!(got.fc_snapshot_version, "v6");
    assert_eq!(got.disk_manifest, row.disk_manifest);
    assert_eq!(got.memory_manifest, row.memory_manifest);
    assert_eq!(got.snapshot_bincode, row.snapshot_bincode);

    // Re-upsert at the same content key with a NEW snapshot_id (a
    // recapture landing under identical content) must overwrite, not
    // duplicate — `content_key` is the primary key.
    let disk_manifest_text = row.disk_manifest.clone();
    let memory_manifest_text = row.memory_manifest.clone();
    let new_snapshot_id = SnapshotId::new();
    let row2 = ColdBaseRow {
        snapshot_id: new_snapshot_id,
        ..row
    };
    meta.upsert_cold_base(row2).await.expect("upsert 2");
    let got2 = meta
        .get_cold_base(&content_key)
        .await
        .expect("get 2")
        .expect("present 2");
    assert_eq!(got2.snapshot_id, new_snapshot_id);

    // The GC pin-set read includes it.
    let ids = meta
        .cold_base_snapshot_ids()
        .await
        .expect("cold base snapshot ids");
    assert!(ids.contains(&new_snapshot_id));

    // ADR 0081 §B6: the chunk-GC pin-set's 7th source — both manifest
    // refs (disk AND memory) must be present, parsed.
    let refs = meta
        .cold_base_manifest_refs()
        .await
        .expect("cold base manifest refs");
    let disk_ref: engram_core::types::manifest::ManifestRef =
        disk_manifest_text.parse().expect("parse disk manifest");
    let mem_ref: engram_core::types::manifest::ManifestRef =
        memory_manifest_text.parse().expect("parse memory manifest");
    assert!(refs.contains(&disk_ref), "disk manifest must be pinned");
    assert!(refs.contains(&mem_ref), "memory manifest must be pinned");
}

/// ADR 0081 §D: `cold_base_fc_version_changed` powers the
/// `recaptured:fc_version_changed` reuse-outcome label — it must find a
/// row for the SAME `disk_manifest` under a DIFFERENT `fc_snapshot_version`,
/// but not when the ONLY row is under the version being checked, and not
/// for an unrelated `disk_manifest`.
#[tokio::test]
#[ignore]
async fn cold_base_fc_version_changed_detects_a_version_drift_on_the_same_disk_manifest() {
    let Some(meta) = connect().await else { return };
    let disk_manifest = format!("{}@v1", Uuid::new_v4());
    let other_disk_manifest = format!("{}@v1", Uuid::new_v4());

    // No row at all yet for either disk manifest.
    assert!(!meta
        .cold_base_fc_version_changed(&disk_manifest, "v10")
        .await
        .expect("no rows yet"));

    let row = ColdBaseRow {
        content_key: format!("test-content-key-{}", Uuid::new_v4()),
        snapshot_id: SnapshotId::new(),
        disk_manifest: disk_manifest.clone(),
        memory_manifest: format!("{}@v1", Uuid::new_v4()),
        fc_snapshot_version: "v9".into(),
        captured_at: Utc::now(),
        snapshot_bincode: vec![1],
    };
    meta.upsert_cold_base(row).await.expect("upsert v9 row");

    // Checking the SAME version the only row was captured under must
    // NOT report drift (there's no OTHER version to have drifted from).
    assert!(!meta
        .cold_base_fc_version_changed(&disk_manifest, "v9")
        .await
        .expect("same version, no drift"));

    // Checking a DIFFERENT version than the recorded row must report
    // drift — this rootfs WAS captured before, just under v9.
    assert!(meta
        .cold_base_fc_version_changed(&disk_manifest, "v10")
        .await
        .expect("different version, drift detected"));

    // An unrelated disk_manifest must never be affected by the v9 row.
    assert!(!meta
        .cold_base_fc_version_changed(&other_disk_manifest, "v10")
        .await
        .expect("unrelated disk manifest, no drift"));
}
