//! Live-Postgres tests for ADR 0046 PG-backed placement reservation, now
//! `MetadataStore::reserve_and_persist_create` (issue #535 (b) — formerly
//! `reserve_placement`)'s `FOR UPDATE` transaction (burst-safe spread +
//! reject), the `mem_budget_mib` ledger column, `fleet_free_mib` (Σ
//! allocatable − reserved), and the one-transaction write-set's atomicity —
//! all against REAL Postgres. The api.rs create tests use a mock store whose
//! `reserve_and_persist_create` is a trivial in-memory stand-in, so this is
//! the only coverage of the actual SQL: the transaction, the reserved-set
//! aggregate, the pending/queued-row insert, the satellite writes (secrets,
//! capabilities, integration policy, harness, selected skills), the `LEFT
//! JOIN`/`GREATEST` free computation, and that migrations 0057/0058/0082
//! apply. Pins the incident fix: a create burst SPREADS across hosts and
//! REJECTS the overflow instead of stacking onto one host (the OOM) — and
//! (issue #535) that the write-set commits as ONE transaction, not a chain
//! of individually-failable writes.
//!
//! `#[ignore]`'d by default; requires Postgres at `ENGRAM_TEST_DATABASE_URL`.
//! Run:
//! ```bash
//! just db-up
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test placement_reservation_live_pg -- --ignored
//! ```

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::{CreateDisposition, MetadataStore, SessionCreateWriteSet};
use engram_core::types::host::{
    HostCapacity, HostMetadata, HostRecord, HostStatus, HostUtilization,
};
use engram_core::types::image::ImageConfig;
use engram_core::types::session::{SessionMode, SessionSpec};
use engram_core::types::{
    Capability, CaptureJobReport, CaptureJobRow, CaptureJobStage, CaptureTerminalReport, HostId,
    NewCaptureJob, SessionId,
};

async fn connect() -> Option<Arc<dyn MetadataStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
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

/// Seed a `ready` host with `allocatable_mib` headroom. `upsert_host` inserts
/// the row (capacity only); `touch_host_heartbeat` writes the utilization that
/// carries `allocatable_mib` (the figure `reserve_placement` reads).
async fn seed_host(meta: &Arc<dyn MetadataStore>, hostname: &str, allocatable_mib: u64) -> HostId {
    let id = HostId::new();
    meta.upsert_host(HostRecord {
        id,
        hostname: hostname.into(),
        cloud_metadata: HostMetadata::default(),
        capacity: zero_capacity(),
        utilization: HostUtilization::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: Some(format!("http://{hostname}:9101")),
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        sandbox_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
        capabilities: engram_core::types::host::HostCapabilities::default(),
    })
    .await
    .expect("upsert host");
    meta.touch_host_heartbeat(
        id,
        engram_core::types::host::HostHeartbeat {
            status: HostStatus::Ready,
            capacity: zero_capacity(),
            utilization: HostUtilization {
                allocatable_mib,
                ..HostUtilization::default()
            },
            ready_images: Vec::new(),
            current_bundles: Vec::new(),
            sandbox_bundles: Vec::new(),
            total_vcpus: 0,
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
        },
    )
    .await
    .expect("heartbeat host");
    id
}

fn spec() -> SessionSpec {
    SessionSpec {
        image: "localhost:5001/placement-reservation:test".into(),
        mode: SessionMode::Agent,
    }
}

fn bare_write_set(
    session_id: SessionId,
    mem_budget_mib: i64,
    cpu_budget_vcpus: i32,
) -> SessionCreateWriteSet {
    SessionCreateWriteSet {
        session_id,
        spec: spec(),
        mem_budget_mib,
        cpu_budget_vcpus,
        sealed_secrets: None,
        capabilities: Vec::new(),
        integration_policy_json: None,
        runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(Vec::new(), None, None),
        oauth_binding: None,
    }
}

/// Issue #535 (b): `reserve_placement` retired — `reserve_and_persist_create`
/// is its structural replacement. Thin wrapper collapsing `CreateDisposition`
/// back to `Option<HostId>` so the pre-existing burst/reject assertions below
/// read the same as before the refactor.
async fn reserve(
    meta: &Arc<dyn MetadataStore>,
    session_id: SessionId,
    mem_budget_mib: i64,
    cpu_budget_vcpus: i32,
    candidates: &[HostId],
    affinity_len: usize,
) -> Option<HostId> {
    match meta
        .reserve_and_persist_create(
            bare_write_set(session_id, mem_budget_mib, cpu_budget_vcpus),
            candidates,
            affinity_len,
        )
        .await
        .expect("reserve_and_persist_create ok")
    {
        CreateDisposition::Placed(h) => Some(h),
        CreateDisposition::Queued => None,
    }
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn burst_packs_one_host_then_overflows_then_rejects() {
    let Some(meta) = connect().await else {
        return;
    };
    // Unique hostnames per run so repeats don't collide on upsert_host's
    // ON CONFLICT (hostname); reserve_placement is candidate-scoped
    // (`host_id = ANY($candidates)`), so this is robust under parallel tests
    // sharing the DB. Two 16 GiB hosts, 4 GiB budgets: exactly 4 fit per host.
    // ADR 0048: placement now PACKS (best-fit) — it fills one host before
    // spilling to the next, so scale-down has a fully-idle host to shed — while
    // still rejecting the overflow (admission control that stopped the OOM).
    let tag = SessionId::new();
    let host_a = seed_host(&meta, &format!("plc-a-{tag}"), 16384).await;
    let host_b = seed_host(&meta, &format!("plc-b-{tag}"), 16384).await;
    let candidates = vec![host_a, host_b];
    let budget = 4096i64;

    let mut placed = Vec::new();
    for _ in 0..8 {
        let picked = reserve(&meta, SessionId::new(), budget, 2, &candidates, 0).await;
        placed.push(picked);
    }
    assert!(
        placed.iter().all(Option::is_some),
        "first 8 (4 per 16 GiB host) must all place; got {placed:?}"
    );
    // Best-fit packs the tie-break-earlier host (host_a) completely before
    // host_b takes any: the first four land on one host, the last four on the
    // other — NOT interleaved.
    let first_four: Vec<_> = placed[..4].iter().map(|h| h.unwrap()).collect();
    assert!(
        first_four.iter().all(|&h| h == first_four[0]),
        "best-fit must PACK one host with the first 4, not spread: {placed:?}"
    );
    let on_a = placed.iter().filter(|h| **h == Some(host_a)).count();
    let on_b = placed.iter().filter(|h| **h == Some(host_b)).count();
    assert_eq!(
        (on_a, on_b),
        (4, 4),
        "both 16 GiB hosts end full (4 each) — packed, not stacked-past-capacity"
    );

    // 9th: both hosts at allocatable (4×4096 = 16384) → free 0 → reject.
    let ninth = reserve(&meta, SessionId::new(), budget, 2, &candidates, 0).await;
    assert_eq!(
        ninth, None,
        "the 9th create must be REJECTED — both hosts are fully reserved (the \
         admission control that stops the OOM)"
    );
}

/// The reserve path's diagnostic twin (2026-07-11 campaign): after a
/// no-fit pick, `placement_no_fit_details` must classify each candidate
/// with the same arithmetic the pick used. Also asserts the SQL (the
/// no-FOR-UPDATE fit read + the reserved UNION incl. capture_jobs)
/// parses and runs against real Postgres.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn placement_no_fit_details_classifies_against_real_pg() {
    let Some(meta) = connect().await else {
        return;
    };
    // One 16 GiB host, fully reserved by one 16 GiB session → a further
    // 4 GiB ask must classify as ram_full with free_mib == 0.
    let host = seed_host(&meta, "no-fit-details-host", 16384).await;
    let sid = SessionId::new();
    let placed = reserve(&meta, sid, 16384, 2, &[host], 0).await;
    assert_eq!(placed, Some(host), "the 16 GiB session reserves the host");
    let details = meta
        .placement_no_fit_details(&[host], 4096, 2)
        .await
        .expect("placement_no_fit_details SQL runs");
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].host_id, host);
    assert_eq!(
        details[0].reason, "ram_full",
        "fully-reserved host must classify ram_full; got {:?}",
        details[0]
    );
    assert_eq!(details[0].free_mib, 0, "16384 alloc − 16384 reserved");
}

/// Issue #535 (b) acceptance criterion: the session write-set ({row,
/// session_secrets, session_capabilities, session_integration_policy,
/// harness, selected_skills}) is committed atomically. Positive half: a
/// SINGLE `reserve_and_persist_create` call with every satellite populated
/// leaves ALL of them visible immediately after — proving they land in one
/// transaction, not a chain of separately-failable writes.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn reserve_and_persist_create_commits_the_full_write_set_together() {
    let Some(meta) = connect().await else {
        return;
    };
    let host = seed_host(&meta, &format!("plc-atomic-{}", SessionId::new()), 16_384).await;
    let session_id = SessionId::new();
    let cap = Capability::parse("github:read@owner/repo").expect("valid capability");
    let ws = SessionCreateWriteSet {
        session_id,
        spec: spec(),
        mem_budget_mib: 2048,
        cpu_budget_vcpus: 1,
        sealed_secrets: Some(engram_core::types::SessionSecrets {
            session_id,
            wrapped_dek: vec![1, 2, 3],
            nonce: vec![4, 5, 6],
            ciphertext: vec![7, 8, 9],
            key_id: "test-key".into(),
            created_at: Utc::now(),
        }),
        capabilities: vec![cap.clone()],
        integration_policy_json: Some(r#"{"network":{"allow_hosts":[]}}"#.into()),
        runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(
            vec!["browser".into()],
            Some("claude".into()),
            None,
        ),
        oauth_binding: None,
    };
    let disposition = meta
        .reserve_and_persist_create(ws, &[host], 0)
        .await
        .expect("reserve_and_persist_create ok");
    assert_eq!(disposition, CreateDisposition::Placed(host));

    // ADR 0077 phase 3: skills are persisted in the RuntimeSpec (written in
    // the create transaction), NOT the retired `sessions.selected_skills`
    // column — assert the RuntimeSpec landed with them.
    let rspec = meta
        .get_session_runtime_spec(session_id)
        .await
        .expect("get_session_runtime_spec")
        .expect("runtime spec written in the create transaction");
    assert_eq!(rspec.selected_skills, vec!["browser".to_string()]);
    let caps = meta
        .get_session_capabilities(session_id)
        .await
        .expect("get_session_capabilities");
    assert_eq!(caps, vec![cap]);
    let policy = meta
        .get_session_integration_policy(session_id)
        .await
        .expect("get_session_integration_policy");
    assert_eq!(policy.as_deref(), Some(r#"{"network":{"allow_hosts":[]}}"#));
    let harness = meta
        .get_session_harness(session_id)
        .await
        .expect("get_session_harness");
    assert_eq!(harness.as_deref(), Some("claude"));
    let secrets = meta
        .get_session_secrets(session_id)
        .await
        .expect("get_session_secrets")
        .expect("secrets row present");
    assert_eq!(secrets.key_id, "test-key");
}

/// Issue #535 (b) acceptance criterion, negative half: when the write-set's
/// OWN row insert fails (a duplicate `session_id` — the only non-idempotent
/// statement in the transaction), NOTHING from that attempt lands — not even
/// satellites a real caller would expect the failed transaction to have
/// written. Simulates "kill the coordinator mid-create" from the DB's point
/// of view: a second, DIFFERENT write-set for the same id never partially
/// applies over the first's.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn reserve_and_persist_create_is_all_or_nothing_on_failure() {
    let Some(meta) = connect().await else {
        return;
    };
    let host = seed_host(&meta, &format!("plc-rollback-{}", SessionId::new()), 16_384).await;
    let session_id = SessionId::new();
    let cap_a = Capability::parse("github:read@owner/repo-a").expect("valid capability");
    let mut ws_a = bare_write_set(session_id, 2048, 1);
    ws_a.capabilities = vec![cap_a.clone()];
    let disposition = meta
        .reserve_and_persist_create(ws_a, &[host], 0)
        .await
        .expect("first reserve_and_persist_create ok");
    assert_eq!(disposition, CreateDisposition::Placed(host));

    // A second call for the SAME session_id: the `INSERT INTO sessions`
    // (no `ON CONFLICT`) violates the primary key and the whole transaction
    // errors — including the satellite writes that would otherwise have
    // followed it in the SAME attempt.
    let cap_b = Capability::parse("github:read@owner/repo-b").expect("valid capability");
    let mut ws_b = bare_write_set(session_id, 4096, 2);
    ws_b.capabilities = vec![cap_b.clone()];
    let second = meta.reserve_and_persist_create(ws_b, &[host], 0).await;
    assert!(
        second.is_err(),
        "a duplicate session_id must fail the whole transaction"
    );

    // The row is exactly what the FIRST call wrote (budgets untouched by the
    // second attempt) …
    let row = meta.get_session(session_id).await.expect("get_session");
    assert_eq!(row.host_id, Some(host));
    // … and the capabilities table carries ONLY the first attempt's
    // capability — the second attempt's `cap_b` never landed, proving the
    // failed transaction didn't leak any of its satellite writes.
    let caps = meta
        .get_session_capabilities(session_id)
        .await
        .expect("get_session_capabilities");
    assert_eq!(
        caps,
        vec![cap_a],
        "the second (failed) attempt's capability must not appear"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn ram_ledger_util_columns_round_trip_through_real_pg() {
    // Issue #540 / migration 0078: `touch_host_heartbeat` writes the RAM
    // ledger's attribution columns (util_base_shm_mib, util_parked_pss_mib,
    // util_running_pss_mib), and `list_active_hosts` (row_from_row) reads
    // them back into the SAME `HostRecord.utilization` `reserve_placement`
    // and the fleet view consume. Pins that the migration applied and the
    // bind/select column lists agree — a drift here would silently zero
    // out attribution on every real coordinator, not just fail a unit test
    // (the pure round-trip is only covered against a fake `HostUtilization`
    // value in engram-protocol's JSON test, never against real SQL).
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let hostname = format!("plc-ledger-{tag}");
    let id = seed_host(&meta, &hostname, 0).await;
    meta.touch_host_heartbeat(
        id,
        engram_core::types::host::HostHeartbeat {
            status: HostStatus::Ready,
            capacity: zero_capacity(),
            utilization: HostUtilization {
                allocatable_mib: 20_000,
                base_shm_mib: 19_500,
                // Not persisted (transient, already folded into
                // allocatable_mib) — must round-trip to 0, not error.
                base_shm_pending_mib: 512,
                parked_pss_mib: 12_288,
                running_pss_mib: 8_000,
                ..HostUtilization::default()
            },
            ready_images: Vec::new(),
            current_bundles: Vec::new(),
            sandbox_bundles: Vec::new(),
            total_vcpus: 0,
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
        },
    )
    .await
    .expect("heartbeat with ram-ledger columns");

    let hosts = meta.list_active_hosts().await.expect("list_active_hosts");
    let row = hosts
        .iter()
        .find(|h| h.id == id)
        .expect("seeded host present in list_active_hosts");
    assert_eq!(row.utilization.allocatable_mib, 20_000);
    assert_eq!(row.utilization.base_shm_mib, 19_500);
    assert_eq!(row.utilization.parked_pss_mib, 12_288);
    assert_eq!(row.utilization.running_pss_mib, 8_000);
    assert_eq!(
        row.utilization.base_shm_pending_mib, 0,
        "base_shm_pending_mib has no PG column and must NOT survive a round-trip"
    );
}

// ---------------------------------------------------------------------
// ADR 0084 (c) — capture placement reservation on `capture_jobs`
//
// #621 (ADR 0081) parked the capture VM's RAM/CPU reservation on
// `enable_jobs`; ADR 0084 (c) moved it onto `capture_jobs` (the durable,
// epoch-fenced job row). Coverage ported from the #621 suite: the
// incident shape (a reserved capture is mutually visible with session
// placement), the waiting → queued-demand → place-on-free flow, epoch
// fencing + IMPLICIT release on a terminal stage, budgets stamped at
// insert (no zero-budget row can exist), and an epoch-fenced reassign
// moving the reservation between hosts.
// ---------------------------------------------------------------------

/// Build an `ImageConfig` whose `resolved_memory_mib`/`resolved_vcpus`
/// derivation yields exactly `(mem_mib, vcpus)` — the single source the
/// capture reservation budgets are stamped from at insert.
fn capture_config(mem_mib: u32, vcpus: u32) -> ImageConfig {
    let mut config = ImageConfig {
        name: "capture-reservation-test".into(),
        ..Default::default()
    };
    config.resources.suggested_memory_mib = Some(mem_mib);
    config.resources.suggested_vcpus = Some(vcpus);
    config
}

/// Seed a fresh enable job + insert a WAITING `capture_jobs` row (budgets
/// stamped from `config`), then attempt the reserving 2D pick over
/// `candidates`. Returns the placed-or-waiting row.
async fn seed_capture_job(
    meta: &Arc<dyn MetadataStore>,
    config: &ImageConfig,
    candidates: &[HostId],
) -> CaptureJobRow {
    let uri = format!("localhost:5001/capture-reservation:{}", SessionId::new());
    let ej = meta
        .create_or_get_enable_job(&uri, None, config)
        .await
        .expect("create enable job");
    let job = meta
        .insert_capture_job(NewCaptureJob {
            enable_job_id: ej.id,
            image_uri: uri,
            manifest_digest: String::new(),
            disk_manifest: format!("{}@v1", SessionId::new()),
            image_config: config.clone(),
            oci_defaults: Default::default(),
            mem_budget_mib: config.resolved_memory_mib() as i64,
            cpu_budget_vcpus: config.resolved_vcpus() as i32,
        })
        .await
        .expect("insert capture job");
    assert_eq!(job.host_id, None, "a fresh capture job inserts WAITING");
    meta.place_capture_job(job.id, candidates)
        .await
        .expect("place capture job")
        .expect("row present")
}

/// Drive a capture job to a terminal `failed` stage — the IMPLICIT
/// reservation release (the row drops out of every reserved-SUM via the
/// `stage NOT IN ('done','failed')` filter; there is no explicit clear).
async fn fail_capture(meta: &Arc<dyn MetadataStore>, job: &CaptureJobRow) {
    let report = CaptureJobReport {
        job_id: job.id,
        epoch: job.epoch,
        stage: CaptureJobStage::Failed,
        progress: None,
        fc_snapshot_version: None,
        terminal: Some(CaptureTerminalReport::Failed {
            error: "test terminal".into(),
            error_stage: "warming".into(),
            retryable: false,
        }),
    };
    assert!(meta
        .record_capture_job_report(&report)
        .await
        .expect("terminal report"));
}

/// (a) The core incident property, both directions: a reserved capture is
/// visible to session placement, and a TERMINAL capture frees the host
/// implicitly (no explicit release call).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn capture_reservation_is_visible_to_session_placement() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host = seed_host(&meta, &format!("cap-a-{tag}"), 16_384).await;

    let job = seed_capture_job(&meta, &capture_config(12_288, 2), &[host]).await;
    assert_eq!(job.host_id, Some(host), "empty host must fit the capture");
    assert_eq!(
        job.mem_budget_mib, 12_288,
        "budget stamped from ImageConfig"
    );

    // An 8 GiB session no longer fits next to the 12 GiB capture VM on a
    // 16 GiB host — the incident shape (capture invisible to session
    // placement) must queue instead.
    let queued = reserve(&meta, SessionId::new(), 8_192, 2, &[host], 0).await;
    assert_eq!(
        queued, None,
        "session placement must see the capture reservation"
    );

    // Terminal the capture → the reserved-SUM drops it implicitly, so the
    // same session-sized reserve now lands. No `clear_capture_reservation`.
    fail_capture(&meta, &job).await;
    let placed = reserve(&meta, SessionId::new(), 8_192, 2, &[host], 0).await;
    assert_eq!(
        placed,
        Some(host),
        "a terminal capture frees the host implicitly"
    );
}

/// (b) The reverse direction + queue semantics: a session reservation
/// blocks the capture; the WAITING capture (host_id NULL) stamps
/// `waiting_since` (kept across re-tries — the timeout clock measures from
/// the FIRST miss), counts as queued demand for the autoscaler, and places
/// once capacity frees.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn waiting_capture_counts_as_queued_demand_and_places_when_freed() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host = seed_host(&meta, &format!("cap-b-{tag}"), 16_384).await;

    // Fill the host with a session first.
    let session = SessionId::new();
    let placed = reserve(&meta, session, 12_288, 2, &[host], 0).await;
    assert_eq!(placed, Some(host));

    // A 7777-MiB capture can't fit → WAITING, wait clock stamped.
    let job = seed_capture_job(&meta, &capture_config(7_777, 2), &[host]).await;
    assert_eq!(
        job.host_id, None,
        "capture must see the session reservation"
    );
    let first_miss = job.waiting_since.expect("wait clock stamped");

    // While waiting, the capture IS queue demand (other suites only ever
    // ADD their own demand rows, so the fleet total is >= our budget).
    let demand = meta.queued_demand().await.expect("queued demand");
    assert!(
        demand.mem_mib >= 7_777,
        "waiting capture must count as queued demand (got {} MiB)",
        demand.mem_mib
    );

    // A re-try while still full keeps the ORIGINAL wait mark (COALESCE).
    let again = meta
        .place_capture_job(job.id, &[host])
        .await
        .expect("re-place")
        .expect("row present");
    assert_eq!(again.host_id, None);
    assert_eq!(
        again.waiting_since,
        Some(first_miss),
        "the timeout clock measures from the FIRST miss"
    );

    // Free the session's reservation → the capture lands and the wait
    // clock clears.
    meta.delete_pending_session(session)
        .await
        .expect("delete pending session");
    let landed = meta
        .place_capture_job(job.id, &[host])
        .await
        .expect("place after free")
        .expect("row present");
    assert_eq!(landed.host_id, Some(host));
    assert_eq!(landed.waiting_since, None);
}

/// (c) Fencing + IMPLICIT release: a stale-epoch reassign can't clobber a
/// live reservation, and a TERMINAL transition releases the host with no
/// explicit clear.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn capture_reservation_is_epoch_fenced_and_released_on_terminal() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host = seed_host(&meta, &format!("cap-c-{tag}"), 16_384).await;

    let job = seed_capture_job(&meta, &capture_config(4_096, 2), &[host]).await;
    assert_eq!(job.host_id, Some(host));

    // A stale-epoch reassign must NOT clobber the live reservation.
    let stale = meta
        .reassign_capture_job(job.id, job.epoch + 7, &[host])
        .await
        .expect("stale reassign call");
    assert!(stale.is_none(), "a stale-epoch reassign must fence off");
    let row = meta
        .get_capture_job(job.id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        row.host_id,
        Some(host),
        "the fenced-off reassign left the reservation intact"
    );

    // Terminal → the host is free again for a full-size session, no
    // explicit `clear_capture_reservation`.
    fail_capture(&meta, &job).await;
    let placed = reserve(&meta, SessionId::new(), 16_000, 2, &[host], 0).await;
    assert_eq!(
        placed,
        Some(host),
        "a terminal capture frees the host implicitly"
    );
}

/// (d) Budgets are ALWAYS stamped at insert from the ImageConfig
/// derivation — no zero-budget row can exist for a real capture (replaces
/// the #621 pre-0095 budget-0-visibility test, which is now impossible:
/// there is no code path that inserts a capture without budgets). Proves
/// the stamped budget is what every reserved-SUM reader sees.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn insert_stamps_real_budgets_from_image_config() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host = seed_host(&meta, &format!("cap-budget-{tag}"), 16_384).await;

    // The derivation is the single source: resolved_* echoes the config.
    let config = capture_config(12_288, 3);
    assert_eq!(config.resolved_memory_mib(), 12_288);
    assert_eq!(config.resolved_vcpus(), 3);

    let job = seed_capture_job(&meta, &config, &[host]).await;
    assert_eq!(job.host_id, Some(host));
    assert_eq!(
        job.mem_budget_mib, 12_288,
        "insert stamped the real mem budget (never 0)"
    );
    assert_eq!(
        job.cpu_budget_vcpus, 3,
        "insert stamped the real vcpu budget (never 0)"
    );

    // Session placement SEES the 12 GiB capture: an 8 GiB session no longer
    // fits (16384 − 12288 = 4096 < 8192). Had the budget been 0, the
    // session would stack onto the capture's host — the OOM.
    let queued = reserve(&meta, SessionId::new(), 8_192, 2, &[host], 0).await;
    assert_eq!(
        queued, None,
        "the stamped capture budget must block the session"
    );

    // …and it appears in `per_host_reserved` with the REAL budget.
    let reserved = meta.per_host_reserved().await.expect("per_host_reserved");
    assert_eq!(
        reserved.get(&host).map(|b| b.mem_mib),
        Some(12_288),
        "per_host_reserved carries the stamped budget, not 0"
    );
}

/// (e) An epoch-fenced reassign MOVES the reservation atomically between
/// hosts (replaces the #621 reclaim-stale-host test — there is no
/// claim-held capture reservation to strand anymore). Host A frees; host B
/// takes the reservation; the swap is one fenced write.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn reassign_moves_the_reservation_between_hosts_under_the_epoch_fence() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host_a = seed_host(&meta, &format!("cap-e-a-{tag}"), 16_384).await;
    let host_b = seed_host(&meta, &format!("cap-e-b-{tag}"), 16_384).await;

    let job = seed_capture_job(&meta, &capture_config(12_288, 2), &[host_a]).await;
    assert_eq!(job.host_id, Some(host_a));

    // The reservation is on host_a: an 8 GiB session there queues.
    assert_eq!(
        reserve(&meta, SessionId::new(), 8_192, 2, &[host_a], 0).await,
        None,
        "the capture reservation blocks host_a"
    );

    // Reassign onto host_b (fresh candidate set): the reserving 2D fit runs
    // inside the epoch fence and the host swaps atomically.
    let moved = meta
        .reassign_capture_job(job.id, job.epoch, &[host_b])
        .await
        .expect("reassign call")
        .expect("reassign lands");
    assert_eq!(moved.host_id, Some(host_b), "reservation moved to host_b");
    assert_eq!(moved.epoch, job.epoch + 1, "epoch bumped");

    // host_a is now free; host_b now carries the reservation.
    assert_eq!(
        reserve(&meta, SessionId::new(), 8_192, 2, &[host_a], 0).await,
        Some(host_a),
        "host_a freed by the move"
    );
    assert_eq!(
        reserve(&meta, SessionId::new(), 8_192, 2, &[host_b], 0).await,
        None,
        "host_b now carries the moved reservation"
    );
}
