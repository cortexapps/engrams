//! Live-Postgres tests for the ADR 0084 `capture_jobs`/`cold_bases`
//! `MetadataStore` surface: insert-or-get dedup (one active job per
//! enable job), the ADR 0084 (c) WAITING-insert + reserving `place`
//! (host_id NULL until an atomic 2D fit binds it), the epoch-fenced
//! report write (stale-epoch rejection, terminal Done/Failed landing,
//! post-terminal immutability), the reserving reassignment/redrive
//! (epoch/attempts bump + host old→new swap under the fence, no-fit ⇒
//! WAITING), the per-stage deadline scan (dispatched-only), the
//! heartbeat-ack-adjacent bulk reads
//! (`capture_assignments_for_host`/`hosts_with_live_capture_jobs`), and
//! the `cold_bases` reuse-lookup round trip.
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
use engram_core::types::host::{
    HostCapacity, HostHeartbeat, HostMetadata, HostRecord, HostStatus, HostUtilization,
};
use engram_core::types::{
    CaptureJobProgress, CaptureJobReport, CaptureJobRow, CaptureJobStage, CaptureTerminalReport,
    ColdBaseRow, NewCaptureJob,
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

/// ADR 0084 (c): a fresh capture job is inserted WAITING (host_id NULL)
/// with its placement budgets stamped — no host is chosen at insert time.
fn new_capture_job(enable_job_id: Uuid) -> NewCaptureJob {
    NewCaptureJob {
        enable_job_id,
        image_uri: format!("test-registry.local/capture-jobs/img/{}", Uuid::new_v4()),
        manifest_digest: "sha256:deadbeef".to_string(),
        disk_manifest: format!("{}@v1", Uuid::new_v4()),
        image_config: test_config(),
        oci_defaults: Default::default(),
        mem_budget_mib: 2_048,
        cpu_budget_vcpus: 2,
    }
}

fn zero_capacity() -> HostCapacity {
    HostCapacity {
        total_gb: 0,
        used_gb: 0,
        total_mib: 0,
        used_mib: 0,
        running_sandboxes: 0,
    }
}

/// Seed a `ready` host with `allocatable_mib` headroom (and a total vCPU
/// count) so the reserving 2D pick (`place_capture_job` /
/// `reassign_capture_job` / `redrive_failed_capture_job` → `pick_host_2d`)
/// can actually fit a capture onto it.
async fn seed_host(
    meta: &Arc<dyn MetadataStore>,
    allocatable_mib: u64,
    total_vcpus: u32,
) -> HostId {
    let id = HostId::new();
    let hostname = format!("cap-host-{id}");
    meta.upsert_host(HostRecord {
        id,
        hostname: hostname.clone(),
        cloud_metadata: HostMetadata::default(),
        capacity: zero_capacity(),
        utilization: HostUtilization::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: Some(format!("http://{hostname}:9101")),
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus,
        wire_version: 0,
        stages_images: false,
        capabilities: engram_core::types::host::HostCapabilities::default(),
    })
    .await
    .expect("upsert host");
    meta.touch_host_heartbeat(
        id,
        HostHeartbeat {
            status: HostStatus::Ready,
            capacity: zero_capacity(),
            utilization: HostUtilization {
                allocatable_mib,
                ..HostUtilization::default()
            },
            ready_images: Vec::new(),
            current_bundles: Vec::new(),
            total_vcpus,
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
        },
    )
    .await
    .expect("heartbeat host");
    id
}

/// Insert a fresh job then place it onto `host` via the reserving pick —
/// the "dispatched capture" the lifecycle tests below drive. Asserts it
/// actually bound (the host must have headroom).
async fn insert_placed(
    meta: &Arc<dyn MetadataStore>,
    enable_job_id: Uuid,
    host: HostId,
) -> CaptureJobRow {
    let job = meta
        .insert_capture_job(new_capture_job(enable_job_id))
        .await
        .expect("insert capture job");
    assert_eq!(job.host_id, None, "a fresh capture job inserts WAITING");
    let placed = meta
        .place_capture_job(job.id, &[host])
        .await
        .expect("place capture job")
        .expect("row present");
    assert_eq!(placed.host_id, Some(host), "place must bind the host");
    placed
}

#[tokio::test]
#[ignore]
async fn insert_capture_job_dedups_active_jobs_per_enable_job() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "dedup").await;

    let a = meta
        .insert_capture_job(new_capture_job(enable_job_id))
        .await
        .expect("insert a");
    assert_eq!(a.stage, CaptureJobStage::Assigned);
    assert_eq!(a.epoch, 1);
    assert_eq!(a.attempts, 1);
    assert_eq!(a.enable_job_id, enable_job_id);

    // Re-insert while the job is still active (e.g. a re-driven scanner
    // tick) → same job, never duplicated.
    let b = meta
        .insert_capture_job(new_capture_job(enable_job_id))
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
        .insert_capture_job(new_capture_job(enable_job_id))
        .await
        .expect("insert c after terminal");
    assert_ne!(c.id, a.id, "a terminal job must not block a fresh capture");
}

#[tokio::test]
#[ignore]
async fn record_capture_job_report_is_fenced_by_epoch() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "fenced").await;
    let job = meta
        .insert_capture_job(new_capture_job(enable_job_id))
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

    // --- Done path ---
    let enable_job_done = seed_enable_job(&meta, "terminal-done").await;
    let job = meta
        .insert_capture_job(new_capture_job(enable_job_done))
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
        .insert_capture_job(new_capture_job(enable_job_failed))
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
    let host_a = seed_host(&meta, 65_536, 64).await;
    let host_b = seed_host(&meta, 65_536, 64).await;
    // Dispatched on host_a via the reserving pick.
    let job = insert_placed(&meta, enable_job_id, host_a).await;

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

    // ADR 0084 (c): reassign re-runs the reserving 2D fit over the
    // candidate set — host_b fits, so the reservation moves atomically.
    let reassigned = meta
        .reassign_capture_job(job.id, job.epoch, &[host_b])
        .await
        .expect("reassign call")
        .expect("reassign lands");
    assert_eq!(reassigned.epoch, job.epoch + 1);
    assert_eq!(reassigned.attempts, job.attempts + 1);
    assert_eq!(reassigned.host_id, Some(host_b));
    assert!(
        reassigned.waiting_since.is_none(),
        "a placed reassign clears the wait clock"
    );
    assert_eq!(reassigned.stage, CaptureJobStage::Assigned);
    assert!(
        reassigned.stage_progress.is_none(),
        "reassignment must clear stale stage progress"
    );

    // A reassign against the now-STALE original epoch must miss.
    let stale = meta
        .reassign_capture_job(job.id, job.epoch, &[host_a])
        .await
        .expect("stale reassign call");
    assert!(stale.is_none(), "a stale-epoch reassign must not land");
}

/// ADR 0084 (c): a reassign that finds NO fitting host leaves the row
/// WAITING (host_id NULL) rather than failing outright — the epoch still
/// bumps (tearing down the stalled attempt host-side) and the row falls
/// into the queue-timeout flow.
#[tokio::test]
#[ignore]
async fn reassign_with_no_fitting_host_leaves_the_row_waiting() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "reassign-nofit").await;
    let host_a = seed_host(&meta, 65_536, 64).await;
    let job = insert_placed(&meta, enable_job_id, host_a).await;

    // Reassign with an EMPTY candidate set: `pick_host_2d` finds nothing.
    let reassigned = meta
        .reassign_capture_job(job.id, job.epoch, &[])
        .await
        .expect("reassign call")
        .expect("row still present (fenced write landed)");
    assert_eq!(
        reassigned.epoch,
        job.epoch + 1,
        "epoch bumps even with no fit"
    );
    assert_eq!(reassigned.host_id, None, "no fit ⇒ WAITING (host_id NULL)");
    assert!(
        reassigned.waiting_since.is_some(),
        "a no-fit reassign stamps the wait anchor",
    );
}

/// ADR 0084 (c): the WAITING flow at the store surface — an insert with no
/// fitting host stays WAITING (host_id NULL, wait anchor stamped), is
/// listed by `list_waiting_capture_jobs` (the queue-timeout scan's read),
/// and `place_capture_job` BINDS it on a later tick once a host fits,
/// clearing the anchor and dropping it out of the waiting list.
#[tokio::test]
#[ignore]
async fn waiting_capture_lists_then_places_on_a_later_tick() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "waiting-reserve-on-tick").await;

    // Insert then place with NO candidate → WAITING.
    let job = meta
        .insert_capture_job(new_capture_job(enable_job_id))
        .await
        .expect("insert");
    let waiting = meta
        .place_capture_job(job.id, &[])
        .await
        .expect("place with no candidate")
        .expect("row present");
    assert_eq!(waiting.host_id, None, "no fit ⇒ WAITING");
    let anchor = waiting
        .waiting_since
        .expect("wait anchor stamped on first miss");

    // The queue-timeout scan's read sees it.
    let listed = meta
        .list_waiting_capture_jobs()
        .await
        .expect("list waiting");
    assert!(
        listed.iter().any(|j| j.id == job.id && j.host_id.is_none()),
        "the waiting job must appear in list_waiting_capture_jobs",
    );

    // A re-tick with STILL no host keeps the ORIGINAL anchor (COALESCE —
    // the queue timeout measures from the FIRST miss).
    let still = meta
        .place_capture_job(job.id, &[])
        .await
        .expect("re-place")
        .expect("row present");
    assert_eq!(
        still.waiting_since,
        Some(anchor),
        "the wait anchor survives re-ticks",
    );

    // Capacity appears → the next tick BINDS it and clears the anchor.
    let host = seed_host(&meta, 65_536, 64).await;
    let placed = meta
        .place_capture_job(job.id, &[host])
        .await
        .expect("place with a fitting host")
        .expect("row present");
    assert_eq!(placed.host_id, Some(host), "a fitting host binds the row");
    assert!(
        placed.waiting_since.is_none(),
        "binding clears the wait anchor"
    );

    // …and it's no longer a waiting row.
    let listed = meta
        .list_waiting_capture_jobs()
        .await
        .expect("list waiting after place");
    assert!(
        !listed.iter().any(|j| j.id == job.id),
        "a placed job must drop out of the waiting list",
    );
}

/// Drive `job` to a TERMINAL `failed`/`retryable=true` row with the
/// per-attempt fields populated (so a subsequent re-drive's clears are
/// observable, not vacuously true).
async fn fail_retryable(meta: &Arc<dyn MetadataStore>, job: &CaptureJobRow) {
    let report = CaptureJobReport {
        job_id: job.id,
        epoch: job.epoch,
        stage: CaptureJobStage::Failed,
        progress: None,
        fc_snapshot_version: Some("v7".into()),
        terminal: Some(CaptureTerminalReport::Failed {
            error: "stream died mid-capture".into(),
            error_stage: "warming".into(),
            retryable: true,
        }),
    };
    assert!(meta
        .record_capture_job_report(&report)
        .await
        .expect("fail-retryable report"));
}

/// Issue #546 blocker: `reassign_capture_job` is fenced `stage NOT IN
/// ('done','failed')` and so silently no-ops on a TERMINAL row — the
/// enable scanner's terminal-retryable arm needs `redrive_failed_capture_job`
/// to actually revive the row. Under budget it must produce a fresh
/// `assigned` attempt (epoch+1/attempts+1, new host, per-attempt fields
/// cleared); `reassign_capture_job` on the same terminal row must NOT.
#[tokio::test]
#[ignore]
async fn redrive_revives_a_retryable_terminal_row_under_budget() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "redrive").await;
    let host_a = seed_host(&meta, 65_536, 64).await;
    let host_b = seed_host(&meta, 65_536, 64).await;
    let job = insert_placed(&meta, enable_job_id, host_a).await;
    fail_retryable(&meta, &job).await;

    // The bug being fixed: `reassign_capture_job` is fenced off a terminal
    // row and matches nothing (the silent no-op that used to loop forever).
    let reassigned = meta
        .reassign_capture_job(job.id, job.epoch, &[host_b])
        .await
        .expect("reassign call");
    assert!(
        reassigned.is_none(),
        "reassign_capture_job must NOT touch a terminal failed row (this is the no-op the fix routes around)"
    );

    // The fix: `redrive_failed_capture_job` under budget revives the row,
    // re-running the reserving 2D fit for the new host (ADR 0084 (c)).
    let redriven = meta
        .redrive_failed_capture_job(job.id, job.epoch, &[host_b], 5)
        .await
        .expect("redrive call")
        .expect("redrive lands under budget");
    assert_eq!(redriven.epoch, job.epoch + 1, "epoch must bump");
    assert_eq!(redriven.attempts, job.attempts + 1, "attempts must bump");
    assert_eq!(redriven.host_id, Some(host_b), "new host must be stamped");
    assert_eq!(redriven.stage, CaptureJobStage::Assigned, "stage resets");
    assert!(redriven.stage_progress.is_none());
    assert!(
        redriven.error.is_none()
            && redriven.error_stage.is_none()
            && redriven.retryable.is_none()
            && redriven.result_bincode.is_none()
            && redriven.fc_snapshot_version.is_none(),
        "per-attempt fields must be cleared, mirroring a fresh insert",
    );

    // The row is now `assigned` at a NEW epoch — a re-drive against the
    // OLD epoch, and any re-drive of a now-non-`failed` row, must miss.
    assert!(
        meta.redrive_failed_capture_job(job.id, job.epoch, &[host_a], 5)
            .await
            .expect("stale-epoch redrive call")
            .is_none(),
        "a stale-epoch redrive must not land",
    );
    assert!(
        meta.redrive_failed_capture_job(job.id, redriven.epoch, &[host_a], 5)
            .await
            .expect("non-failed redrive call")
            .is_none(),
        "a re-drive of a non-`failed` row must miss",
    );
}

/// The attempts budget is atomic in the verb: a retryable-terminal row
/// whose `attempts` has reached the budget refuses the re-drive (0 rows →
/// `None`), leaving the row `failed` so the enable scanner fails the
/// enable job with the terminal row's own failure kind.
#[tokio::test]
#[ignore]
async fn redrive_refused_when_attempts_at_budget() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "redrive-budget").await;
    let host_a = seed_host(&meta, 65_536, 64).await;
    let job = insert_placed(&meta, enable_job_id, host_a).await;
    // A fresh row is at attempts = 1. Fail it retryable, then re-drive
    // with a budget of exactly 1: `attempts < 1` is false, so the fence
    // matches nothing (candidates are irrelevant — the budget fence fails
    // before any placement is attempted).
    fail_retryable(&meta, &job).await;
    assert_eq!(job.attempts, 1);
    let refused = meta
        .redrive_failed_capture_job(job.id, job.epoch, &[], 1)
        .await
        .expect("at-budget redrive call");
    assert!(
        refused.is_none(),
        "a re-drive at the attempts budget must be refused",
    );

    // The row stays terminal `failed` (unchanged) — the enable scanner's
    // fail path can now surface it as a terminal enable-job failure.
    let got = meta.get_capture_job(job.id).await.unwrap().unwrap();
    assert_eq!(got.stage, CaptureJobStage::Failed);
    assert_eq!(got.attempts, 1, "a refused re-drive must not bump attempts");
    assert_eq!(got.error.as_deref(), Some("stream died mid-capture"));
}

#[tokio::test]
#[ignore]
async fn expire_capture_job_stages_picks_up_only_over_budget_rows() {
    let Some(meta) = connect().await else { return };
    // ADR 0084 (c): `expire_capture_job_stages` is DISPATCHED-only
    // (host_id NOT NULL), so both jobs must be placed on a real host.
    let host = seed_host(&meta, 65_536, 64).await;
    let enable_job_assigned = seed_enable_job(&meta, "expire-assigned").await;
    let enable_job_booting = seed_enable_job(&meta, "expire-booting").await;

    let assigned_job = insert_placed(&meta, enable_job_assigned, host).await;
    let booting_job = insert_placed(&meta, enable_job_booting, host).await;
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
    let host = seed_host(&meta, 65_536, 64).await;
    let other_host = seed_host(&meta, 65_536, 64).await;

    let enable_job_active = seed_enable_job(&meta, "assignments-active").await;
    let enable_job_done = seed_enable_job(&meta, "assignments-done").await;
    let enable_job_other = seed_enable_job(&meta, "assignments-other-host").await;

    let active = insert_placed(&meta, enable_job_active, host).await;
    let done = insert_placed(&meta, enable_job_done, host).await;
    let other = insert_placed(&meta, enable_job_other, other_host).await;

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
    let live_host = seed_host(&meta, 65_536, 64).await;
    let done_host = seed_host(&meta, 65_536, 64).await;

    let enable_job_live = seed_enable_job(&meta, "live-host").await;
    let enable_job_done = seed_enable_job(&meta, "done-host").await;

    insert_placed(&meta, enable_job_live, live_host).await;
    let done_job = insert_placed(&meta, enable_job_done, done_host).await;
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

    // ADR 0084 §B6: the chunk-GC pin-set's 7th source — both manifest
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

/// ADR 0084 §D: `cold_base_fc_version_changed` powers the
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

/// ADR 0088: a host-bound, non-terminal capture job counts in the
/// per-host live-work aggregate the operator's roll/drain gates read —
/// and drops out the moment the job reaches a terminal stage. (WAITING
/// rows bind no host and are covered by the placement tests above.)
#[tokio::test]
#[ignore]
async fn placed_capture_job_counts_as_live_enable_work() {
    let Some(meta) = connect().await else { return };
    let enable_job_id = seed_enable_job(&meta, "live-work").await;
    let host = seed_host(&meta, 16_384, 8).await;
    let placed = insert_placed(&meta, enable_job_id, host).await;

    let work = meta
        .live_enable_work_by_host(std::time::Duration::from_secs(300))
        .await
        .expect("live work");
    assert_eq!(
        work.get(&host).map(|w| w.captures),
        Some(1),
        "a placed non-terminal capture counts: {work:?}"
    );

    let done = CaptureJobReport {
        job_id: placed.id,
        epoch: placed.epoch,
        stage: CaptureJobStage::Done,
        progress: None,
        fc_snapshot_version: Some("v6".into()),
        terminal: Some(CaptureTerminalReport::Done {
            result_bincode: vec![1],
        }),
    };
    assert!(meta
        .record_capture_job_report(&done)
        .await
        .expect("terminal report"));

    let work = meta
        .live_enable_work_by_host(std::time::Duration::from_secs(300))
        .await
        .expect("live work");
    assert_eq!(
        work.get(&host).map(|w| w.captures).unwrap_or(0),
        0,
        "a terminal capture releases the host: {work:?}"
    );
}
