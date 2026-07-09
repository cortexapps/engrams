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

use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::{
    CaptureReservation, CreateDisposition, MetadataStore, SessionCreateWriteSet,
};
use engram_core::types::host::{
    HostCapacity, HostMetadata, HostRecord, HostStatus, HostUtilization,
};
use engram_core::types::session::{SessionMode, SessionSpec};
use engram_core::types::{Capability, HostId, SessionId};

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

/// A raw single-connection pool on the same DB — used by the ADR 0081
/// tests to (a) force a job into the pre-0095 `mem_budget_mib = 0` shape
/// and (b) stamp a `capture_waiting_since` anchor alongside a live
/// `capture_host_id` (a combination the public API keeps mutually
/// exclusive). Only ever called after `connect()` proved the env var is
/// set. Single connection + explicit `close()` keeps the shared,
/// serially-run (`--test-threads=1`) DB from exhausting its connection
/// budget across the suite.
async fn raw_pool() -> sqlx::PgPool {
    let url = std::env::var("ENGRAM_TEST_DATABASE_URL").expect("ENGRAM_TEST_DATABASE_URL set");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("raw pool connect")
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

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn fleet_free_mib_sql_runs_against_real_pg() {
    let Some(meta) = connect().await else {
        return;
    };
    // The exact value is global (all ready hosts) and unit-tested via the pure
    // decision fn; here we only assert the SQL — the LEFT JOIN over the
    // reserved aggregate, GREATEST(0, …), the pending age-guard, the ::BIGINT
    // cast — parses and runs against real Postgres and yields a sane figure.
    let free = meta
        .fleet_free_mib()
        .await
        .expect("fleet_free_mib SQL runs");
    assert!(
        free >= 0,
        "free_mib is a non-negative MiB count; got {free}"
    );
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
// ADR 0081 — capture placement rides the session scheduler
// ---------------------------------------------------------------------

/// Collapse a [`CaptureReservation`] to `Option<HostId>` so the
/// fit/no-fit assertions below read the same as before the return type
/// carried the wait anchor.
fn reserved_host(r: CaptureReservation) -> Option<engram_core::types::HostId> {
    match r {
        CaptureReservation::Reserved(h) => Some(h),
        CaptureReservation::Waiting { .. } => None,
    }
}

/// Create + claim an enable job whose config budgets are
/// (`mem_mib`, `vcpus`), returning it with the claim held by `claimant`.
/// Unique `image_uri` per call: the partial unique index allows one
/// non-terminal job per uri, and the shared test DB runs suites
/// concurrently.
async fn seed_claimed_job(
    meta: &Arc<dyn MetadataStore>,
    claimant: &str,
    mem_mib: u32,
    vcpus: u32,
) -> engram_core::types::EnableJob {
    let mut config = engram_core::types::image::ImageConfig {
        name: "capture-reservation-test".into(),
        ..Default::default()
    };
    config.resources.suggested_memory_mib = Some(mem_mib);
    config.resources.suggested_vcpus = Some(vcpus);
    let uri = format!("localhost:5001/capture-reservation:{}", SessionId::new());
    let job = meta
        .create_or_get_enable_job(&uri, None, &config)
        .await
        .expect("create enable job");
    // Claim broadly; our fresh job is unclaimed so it's in the batch.
    // (Other suites' jobs may be claimed too — fenced writes only touch
    // OUR job id, so that's harmless.)
    meta.claim_enable_jobs(claimant, 300, 64)
        .await
        .expect("claim enable jobs");
    meta.get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present")
}

/// The core ADR 0081 property, both directions: a reserved capture is
/// visible to session placement, and releasing it frees the host.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn capture_reservation_is_visible_to_session_placement() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host = seed_host(&meta, &format!("cap-a-{tag}"), 16_384).await;
    let claimant = format!("t-{tag}");

    let job = seed_claimed_job(&meta, &claimant, 12_288, 2).await;
    // The budgets were stamped at creation from the image config — the
    // same derivation sessions reserve with.
    assert_eq!(job.mem_budget_mib, 12_288);
    assert_eq!(job.cpu_budget_vcpus, 2);

    let picked = reserved_host(
        meta.reserve_capture_host(job.id, &claimant, &[host], job.mem_budget_mib, 2)
            .await
            .expect("reserve capture host"),
    );
    assert_eq!(picked, Some(host), "empty host must fit the capture");

    // A 8 GiB session no longer fits next to the 12 GiB capture VM on a
    // 16 GiB host — the incident shape (capture invisible to session
    // placement) must queue instead.
    let queued = reserve(&meta, SessionId::new(), 8_192, 2, &[host], 0).await;
    assert_eq!(
        queued, None,
        "session placement must see the capture reservation"
    );

    // Release → the same session-sized reserve now lands.
    meta.clear_capture_reservation(job.id, &claimant)
        .await
        .expect("clear capture reservation");
    let placed = reserve(&meta, SessionId::new(), 8_192, 2, &[host], 0).await;
    assert_eq!(placed, Some(host), "released capture frees the host");

    let row = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(row.capture_host_id, None);
    assert_eq!(row.capture_waiting_since, None);
}

/// The reverse direction + the queue semantics: a session reservation
/// blocks the capture; the waiting capture stamps
/// `capture_waiting_since` (kept across re-tries — the timeout clock
/// measures from the FIRST miss), counts as queued demand for the
/// autoscaler, and places once capacity frees.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn waiting_capture_counts_as_queued_demand_and_places_when_freed() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host = seed_host(&meta, &format!("cap-b-{tag}"), 16_384).await;
    let claimant = format!("t-{tag}");

    // Fill the host with a session first.
    let session = SessionId::new();
    let placed = reserve(&meta, session, 12_288, 2, &[host], 0).await;
    assert_eq!(placed, Some(host));

    // A 7777-MiB capture can't fit → waiting, wait clock stamped.
    let job = seed_claimed_job(&meta, &claimant, 7_777, 2).await;
    let picked = reserved_host(
        meta.reserve_capture_host(job.id, &claimant, &[host], 7_777, 2)
            .await
            .expect("reserve capture host"),
    );
    assert_eq!(picked, None, "capture must see the session reservation");
    let row = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(row.capture_host_id, None);
    let first_miss = row.capture_waiting_since.expect("wait clock stamped");

    // While waiting, the capture IS queue demand (other suites only ever
    // ADD their own demand rows, so the fleet total is >= our budget).
    let demand = meta.queued_demand().await.expect("queued demand");
    assert!(
        demand.mem_mib >= 7_777,
        "waiting capture must count as queued demand (got {} MiB)",
        demand.mem_mib
    );

    // A re-try while still full keeps the ORIGINAL wait mark (COALESCE).
    let again = reserved_host(
        meta.reserve_capture_host(job.id, &claimant, &[host], 7_777, 2)
            .await
            .expect("re-reserve"),
    );
    assert_eq!(again, None);
    let row = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(
        row.capture_waiting_since,
        Some(first_miss),
        "the timeout clock measures from the FIRST miss"
    );

    // Free the session's reservation → the capture lands and the wait
    // clock clears.
    meta.delete_pending_session(session)
        .await
        .expect("delete pending session");
    let picked = reserved_host(
        meta.reserve_capture_host(job.id, &claimant, &[host], 7_777, 2)
            .await
            .expect("reserve after free"),
    );
    assert_eq!(picked, Some(host));
    let row = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(row.capture_host_id, Some(host));
    assert_eq!(row.capture_waiting_since, None);
}

/// Fencing + the failure path: a stale claimant can't reserve, and
/// `record_enable_job_failure` (which releases the claim) also releases
/// the capture reservation in the same write.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn capture_reservation_is_fenced_and_released_on_failure() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host = seed_host(&meta, &format!("cap-c-{tag}"), 16_384).await;
    let claimant = format!("t-{tag}");

    let job = seed_claimed_job(&meta, &claimant, 4_096, 2).await;
    let err = meta
        .reserve_capture_host(job.id, "someone-else", &[host], 4_096, 2)
        .await
        .expect_err("stale claimant must not reserve");
    assert!(
        matches!(err, engram_core::MetaError::Conflict(_)),
        "fence miss must be a Conflict, got {err:?}"
    );

    let picked = reserved_host(
        meta.reserve_capture_host(job.id, &claimant, &[host], 4_096, 2)
            .await
            .expect("reserve with the real claimant"),
    );
    assert_eq!(picked, Some(host));

    // The failure record releases claim AND reservation in ONE write.
    meta.record_enable_job_failure(job.id, &claimant, "test failure", 5, false)
        .await
        .expect("record failure");
    let row = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(row.capture_host_id, None);
    assert_eq!(row.capture_waiting_since, None);

    // The host is free again for a full-size session.
    let placed = reserve(&meta, SessionId::new(), 16_000, 2, &[host], 0).await;
    assert_eq!(placed, Some(host));
}

/// ADR 0081 (fix — MAJOR): a pre-0095 job row carries `mem_budget_mib = 0`.
/// The caller resolves fallback budgets from the image config and passes
/// them to `reserve_capture_host`, which must PERSIST them on the row —
/// else every OTHER placer's reserved-SUM (`pick_host_2d`,
/// `per_host_reserved`, `fleet_free_mib`) sees the 24 GiB-class capture as
/// 0 MiB reserved (the exact 2026-07-08 OOM class), and a budget-0 WAITING
/// job would fold 0 into `queued_demand` so the autoscaler never grows for
/// it. Proves the persisted row carries the budgets AND that session
/// placement sees the reservation.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn fallback_budgets_persist_and_are_visible_to_session_placement() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    let host = seed_host(&meta, &format!("cap-budget0-{tag}"), 16_384).await;
    let claimant = format!("t-{tag}");

    // Create + claim a job, then FORCE its row into the pre-migration
    // shape (budgets 0) — the exact case the config fallback exists for.
    let job = seed_claimed_job(&meta, &claimant, 12_288, 2).await;
    let pool = raw_pool().await;
    sqlx::query("UPDATE enable_jobs SET mem_budget_mib = 0, cpu_budget_vcpus = 0 WHERE id = $1")
        .bind(job.id)
        .execute(&pool)
        .await
        .expect("zero the budgets (simulate pre-0095 row)");
    let zeroed = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(zeroed.mem_budget_mib, 0, "row now looks pre-0095");

    // The caller's fallback path: resolve budgets from config and reserve
    // with them. `reserve_capture_host` must stamp them onto the row.
    let fallback_mem = 12_288i64;
    let picked = reserved_host(
        meta.reserve_capture_host(job.id, &claimant, &[host], fallback_mem, 2)
            .await
            .expect("reserve with fallback budgets"),
    );
    assert_eq!(picked, Some(host), "empty host fits the capture");

    // The reservation PERSISTED the budgets (not left at 0).
    let row = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(
        row.mem_budget_mib, fallback_mem,
        "the fallback mem budget was persisted onto the row"
    );
    assert_eq!(
        row.cpu_budget_vcpus, 2,
        "the fallback vcpu budget was persisted onto the row"
    );
    assert_eq!(row.capture_host_id, Some(host));

    // Session placement now SEES the 12 GiB capture: an 8 GiB session no
    // longer fits the 16 GiB host (16384 − 12288 = 4096 < 8192). Had the
    // budget stayed 0, the reserved-SUM would read 0 and the session would
    // stack onto the capture's host — the OOM.
    let queued = reserve(&meta, SessionId::new(), 8_192, 2, &[host], 0).await;
    assert_eq!(
        queued, None,
        "the reserved capture (persisted budget) must block the session"
    );

    // …and it appears in `per_host_reserved` with the REAL budget.
    let reserved = meta.per_host_reserved().await.expect("per_host_reserved");
    assert_eq!(
        reserved.get(&host).map(|b| b.mem_mib),
        Some(fallback_mem),
        "per_host_reserved carries the persisted budget, not 0"
    );

    pool.close().await;
}

/// ADR 0081 (fix — MAJOR): a pod that reserves a host then crashes / loses
/// its lease leaves a stale `capture_host_id`. `claim_enable_jobs` clears
/// it on (re-)claim, so the re-running pod's `reserve_capture_host`
/// doesn't count the job's OWN phantom reservation against the only viable
/// host — which on a tight fleet is an un-fittable self-block (→ 30-min
/// non-retryable `CapacityTimeout`, and `queued_demand` excludes it so the
/// autoscaler won't help). `capture_waiting_since` (the first-miss wait
/// anchor the timeout measures from) MUST survive the reclaim.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn reclaim_clears_stale_capture_host_but_keeps_wait_anchor() {
    let Some(meta) = connect().await else {
        return;
    };
    let tag = SessionId::new();
    // The 16 GiB host fits EXACTLY one 12 GiB capture; a self-counted
    // phantom would leave only 4 GiB free and block the re-reserve.
    let host = seed_host(&meta, &format!("cap-reclaim-{tag}"), 16_384).await;
    let pod_a = format!("pod-a-{tag}");

    let job = seed_claimed_job(&meta, &pod_a, 12_288, 2).await;
    let picked = reserved_host(
        meta.reserve_capture_host(job.id, &pod_a, &[host], 12_288, 2)
            .await
            .expect("pod-a reserves the host"),
    );
    assert_eq!(picked, Some(host));

    // Stamp a first-miss wait anchor alongside the live reservation (the
    // job had waited before it fit) so we can prove the anchor survives
    // reclaim — a combination the public API keeps mutually exclusive, so
    // set it directly.
    let pool = raw_pool().await;
    let anchor: chrono::DateTime<Utc> = sqlx::query_scalar(
        "UPDATE enable_jobs SET capture_waiting_since = NOW() - INTERVAL '5 minutes' \
         WHERE id = $1 RETURNING capture_waiting_since",
    )
    .bind(job.id)
    .fetch_one(&pool)
    .await
    .expect("stamp wait anchor");

    // Pod A crashes; pod B re-claims the expired lease (0-second lease =
    // instant expiry — the `enable_jobs_live_pg` steal pattern).
    let pod_b = format!("pod-b-{tag}");
    meta.claim_enable_jobs(&pod_b, 0, 64)
        .await
        .expect("pod-b re-claims the expired lease");

    let row = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(
        row.capture_host_id, None,
        "re-claim clears the crashed pod's stale capture reservation"
    );
    assert_eq!(
        row.capture_waiting_since,
        Some(anchor),
        "the first-miss wait anchor survives reclaim (the timeout measures from it)"
    );

    // The re-running pod re-reserves from scratch: with the phantom
    // cleared, the 12 GiB capture fits the 16 GiB host again. Without the
    // claim-time clear it would count its own stale 12 GiB and never fit.
    let repicked = reserved_host(
        meta.reserve_capture_host(job.id, &pod_b, &[host], 12_288, 2)
            .await
            .expect("pod-b re-reserves"),
    );
    assert_eq!(
        repicked,
        Some(host),
        "no self-counting: the capture fits the host it previously held"
    );

    // The fit also cleared the wait clock.
    let row = meta
        .get_enable_job(job.id)
        .await
        .expect("get job")
        .expect("job present");
    assert_eq!(row.capture_host_id, Some(host));
    assert_eq!(
        row.capture_waiting_since, None,
        "a fit stops the job counting as waiting demand"
    );

    pool.close().await;
}
