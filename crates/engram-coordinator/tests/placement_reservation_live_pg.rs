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
use engram_core::traits::{CreateDisposition, MetadataStore, SessionCreateWriteSet};
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
        local_snapshots: Vec::new(),
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
            local_snapshots: Vec::new(),
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
        selected_harness: None,
        selected_skills: Vec::new(),
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
        selected_harness: Some("claude".into()),
        selected_skills: vec!["browser".into()],
    };
    let disposition = meta
        .reserve_and_persist_create(ws, &[host], 0)
        .await
        .expect("reserve_and_persist_create ok");
    assert_eq!(disposition, CreateDisposition::Placed(host));

    let row = meta.get_session(session_id).await.expect("get_session");
    assert_eq!(row.selected_skills, vec!["browser".to_string()]);
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
            local_snapshots: Vec::new(),
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
