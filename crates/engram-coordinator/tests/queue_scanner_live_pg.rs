//! Live-Postgres tests for the ADR 0048 session-queue store layer:
//! `enqueue_session_create`, `list_queued_sessions_fifo`,
//! `place_queued_session` (the queued-row reservation transaction),
//! `requeue_session`, `requeue_stale_pending`, and `queued_demand` — all
//! against REAL Postgres (the migration 0064 schema + the FIFO index +
//! the queued→pending flip). The scanner's boot half is covered by the
//! api.rs create tests + e2e_stack; this pins the SQL the scanner stands on.
//!
//! `#[ignore]`'d by default; requires Postgres at `ENGRAM_TEST_DATABASE_URL`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::host::{
    HostCapacity, HostHeartbeat, HostRecord, HostStatus, HostUtilization,
};
use engram_core::types::session::{QueueOrigin, SessionMode, SessionSpec, SessionState};
use engram_core::types::{HostId, SessionId};

async fn connect() -> Option<Arc<dyn MetadataStore>> {
    let url = std::env::var("ENGRAM_TEST_DATABASE_URL").ok()?;
    let store = engram_postgres::PostgresStore::connect(&url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    Some(Arc::new(store))
}

fn spec() -> SessionSpec {
    SessionSpec {
        image: format!("localhost:5001/queue-test:{}", uuid::Uuid::new_v4()),
        mode: SessionMode::Agent,
        user_id: None,
    }
}

async fn seed_ready_host(
    meta: &Arc<dyn MetadataStore>,
    allocatable_mib: u64,
    total_vcpus: u32,
) -> HostId {
    let id = HostId::new();
    let name = format!("q-{id}");
    meta.upsert_host(HostRecord {
        id,
        hostname: name,
        cloud_metadata: Default::default(),
        capacity: HostCapacity {
            total_gb: 0,
            used_gb: 0,
            total_mib: allocatable_mib,
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
    })
    .await
    .expect("upsert host");
    meta.touch_host_heartbeat(
        id,
        HostHeartbeat {
            status: HostStatus::Ready,
            capacity: HostCapacity {
                total_gb: 0,
                used_gb: 0,
                total_mib: allocatable_mib,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: HostUtilization {
                allocatable_mib,
                ..HostUtilization::default()
            },
            ready_images: Vec::new(),
            local_snapshots: Vec::new(),
            current_bundles: Vec::new(),
            total_vcpus,
            // Issue #229: report the coordinator's wire version so the
            // placement filter keeps this seeded host schedulable.
            wire_version: engram_protocol::WIRE_VERSION,
        },
    )
    .await
    .expect("heartbeat host");
    id
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn enqueue_list_demand_and_fifo_order() {
    let Some(meta) = connect().await else { return };

    let s1 = SessionId::new();
    let s2 = SessionId::new();
    // s1 enqueued first → must sort first (FIFO by queued_at).
    meta.enqueue_session_create(s1, &spec(), 4096, 2, Some("hello"))
        .await
        .expect("enqueue s1");
    tokio::time::sleep(Duration::from_millis(10)).await;
    meta.enqueue_session_create(s2, &spec(), 8192, 4, None)
        .await
        .expect("enqueue s2");

    // demand reflects both (Σ over ALL queued in the shared DB ≥ ours).
    let demand = meta.queued_demand().await.expect("demand");
    assert!(demand.sessions >= 2);
    assert!(demand.mem_mib >= 4096 + 8192);
    assert!(demand.vcpus >= 2 + 4);

    let queued = meta.list_queued_sessions_fifo().await.expect("list");
    let ours: Vec<_> = queued
        .iter()
        .filter(|q| q.session.id == s1 || q.session.id == s2)
        .collect();
    assert_eq!(ours.len(), 2);
    // FIFO: s1 (older queued_at) appears before s2 within our pair.
    let i1 = queued.iter().position(|q| q.session.id == s1).unwrap();
    let i2 = queued.iter().position(|q| q.session.id == s2).unwrap();
    assert!(i1 < i2, "FIFO: s1 enqueued first must come before s2");
    // Metadata round-trips.
    let q1 = ours.iter().find(|q| q.session.id == s1).unwrap();
    assert_eq!(q1.origin, QueueOrigin::Create);
    assert_eq!(q1.prompt.as_deref(), Some("hello"));
    assert_eq!(q1.mem_budget_mib, 4096);
    assert_eq!(q1.cpu_budget_vcpus, 2);
    assert_eq!(q1.session.status, SessionState::Queued);
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn place_queued_flips_to_pending_on_a_fitting_host() {
    let Some(meta) = connect().await else { return };
    let host = seed_ready_host(&meta, 16_384, 8).await;
    let sid = SessionId::new();
    meta.enqueue_session_create(sid, &spec(), 4096, 2, None)
        .await
        .expect("enqueue");

    // Fits → flips queued → pending, binds the host, returns it.
    let placed = meta
        .place_queued_session(sid, 4096, 2, &[host], 0)
        .await
        .expect("place");
    assert_eq!(placed, Some(host));
    let row = meta.get_session(sid).await.expect("get");
    assert_eq!(row.status, SessionState::Pending);
    assert_eq!(row.host_id, Some(host));

    // A second place is a clean no-op (row already left `queued`).
    let again = meta
        .place_queued_session(sid, 4096, 2, &[host], 0)
        .await
        .expect("place again");
    assert_eq!(again, None);
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn place_queued_returns_none_when_no_host_fits() {
    let Some(meta) = connect().await else { return };
    // Host with only 2 GiB allocatable; a 4 GiB session can't fit.
    let host = seed_ready_host(&meta, 2048, 8).await;
    let sid = SessionId::new();
    meta.enqueue_session_create(sid, &spec(), 4096, 2, None)
        .await
        .expect("enqueue");
    let placed = meta
        .place_queued_session(sid, 4096, 2, &[host], 0)
        .await
        .expect("place");
    assert_eq!(placed, None, "no host fits → stay queued");
    let row = meta.get_session(sid).await.expect("get");
    assert_eq!(row.status, SessionState::Queued, "row stays queued");
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn requeue_and_stale_pending_recovery() {
    let Some(meta) = connect().await else { return };
    let host = seed_ready_host(&meta, 16_384, 8).await;
    let sid = SessionId::new();
    meta.enqueue_session_create(sid, &spec(), 4096, 2, None)
        .await
        .expect("enqueue");
    meta.place_queued_session(sid, 4096, 2, &[host], 0)
        .await
        .expect("place");

    // requeue_session: pending → queued.
    assert!(meta.requeue_session(sid).await.expect("requeue"));
    assert_eq!(
        meta.get_session(sid).await.unwrap().status,
        SessionState::Queued
    );
    // A second requeue is a no-op (no longer pending).
    assert!(!meta.requeue_session(sid).await.expect("requeue2"));

    // requeue_stale_pending: place again, then reclaim with a zero
    // staleness window (everything older than "now" qualifies).
    meta.place_queued_session(sid, 4096, 2, &[host], 0)
        .await
        .expect("place2");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let n = meta
        .requeue_stale_pending(Duration::from_millis(1))
        .await
        .expect("stale");
    assert!(n >= 1, "stale placed-pending row should be reclaimed");
    assert_eq!(
        meta.get_session(sid).await.unwrap().status,
        SessionState::Queued
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn resume_origin_enqueue_requires_idle() {
    let Some(meta) = connect().await else { return };
    // A non-idle session is a no-op for the resume enqueue (gated on
    // status='idle'); we just assert it doesn't error and doesn't queue.
    let sid = SessionId::new();
    meta.enqueue_session_create(sid, &spec(), 4096, 2, None)
        .await
        .expect("enqueue create");
    // It's `queued`, not `idle`, so enqueue_session_resume is a no-op.
    meta.enqueue_session_resume(sid)
        .await
        .expect("resume enqueue no-op");
    let row = meta.get_session(sid).await.unwrap();
    // Still a create-origin queued row.
    assert_eq!(row.status, SessionState::Queued);
}
