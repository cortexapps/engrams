//! Live-Postgres regression for issue #230: host lifecycle transition
//! discipline. The dead-host sweep
//! (`mark_host_dead_and_orphan_sessions`) marks a partitioned host
//! `dead` and unbinds its sessions. Before the fix, the host's very
//! next heartbeat did a blind `UPDATE hosts SET status = $2` and flipped
//! `dead -> ready`, resurrecting a zombie host into the schedulable set
//! while its former sessions sat unbound. The guard in
//! `touch_host_heartbeat` pins `dead` (mirroring
//! `HostStatus::can_transition_to`, where `dead` is terminal): a `dead`
//! row returns to `ready` ONLY via an explicit re-register, never on a
//! heartbeat.
//!
//! `#[ignore]`'d by default; requires Postgres at `ENGRAM_TEST_DATABASE_URL`.
//! Run:
//! ```bash
//! just db-up
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test host_status_transition_live_pg -- --ignored
//! ```

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::host::{
    HostCapacity, HostHeartbeat, HostMetadata, HostRecord, HostStatus, HostUtilization,
};
use engram_core::types::HostId;
use engram_postgres::PostgresStore;

async fn connect() -> Option<PostgresStore> {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skipping: ENGRAM_TEST_DATABASE_URL not set (run `just db-up`)");
            return None;
        }
    };
    let store = PostgresStore::connect(&database_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");
    Some(store)
}

fn host(id: HostId, hostname: &str, addr: &str) -> HostRecord {
    HostRecord {
        id,
        hostname: hostname.to_string(),
        cloud_metadata: HostMetadata::default(),
        capacity: HostCapacity {
            total_gb: 100,
            used_gb: 1,
            total_mib: 65_536,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: Some(addr.to_string()),
        ready_images: Vec::new(),
        local_snapshots: Vec::new(),
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
    }
}

fn heartbeat(status: HostStatus) -> HostHeartbeat {
    HostHeartbeat {
        status,
        capacity: HostCapacity {
            total_gb: 0,
            used_gb: 0,
            total_mib: 65_536,
            used_mib: 1_024,
            running_sandboxes: 1,
        },
        utilization: HostUtilization::default(),
        ready_images: Vec::new(),
        local_snapshots: Vec::new(),
        current_bundles: Vec::new(),
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
    }
}

async fn read_status(store: &PostgresStore, id: HostId) -> String {
    let row: (String,) = sqlx::query_as(r#"SELECT status FROM hosts WHERE id = $1"#)
        .bind(id.as_uuid())
        .fetch_one(store.pool())
        .await
        .expect("read host status");
    row.0
}

/// The core invariant: once a host is `dead`, no heartbeat — whatever
/// `status` it self-reports (`ready` OR `draining`) — may flip it back.
/// The row stays `dead` until an explicit re-register.
#[tokio::test]
#[ignore = "requires live Postgres (ENGRAM_TEST_DATABASE_URL)"]
async fn heartbeat_cannot_resurrect_a_dead_host() {
    let Some(store) = connect().await else { return };
    let id = HostId::new();
    let hostname = format!("hf-host-agent-test-{}", &id.to_string()[..8]);

    store
        .upsert_host(host(id, &hostname, "http://10.0.0.1:9101"))
        .await
        .expect("register host (status=ready)");

    // The dead-host sweep marks the partitioned host dead + orphans its
    // sessions (none here — the host-status flip is what we're asserting).
    store
        .mark_host_dead_and_orphan_sessions(id)
        .await
        .expect("mark dead");
    assert_eq!(
        read_status(&store, id).await,
        "dead",
        "sweep marked it dead"
    );

    // A merely-partitioned host keeps heartbeating. Each tick must be a
    // no-op on `status`, NOT a `dead -> ready` resurrection — try both a
    // ready-reporting heartbeat and a draining one.
    for hb_status in [HostStatus::Ready, HostStatus::Draining] {
        store
            .touch_host_heartbeat(id, heartbeat(hb_status))
            .await
            .expect("heartbeat persists (capacity/utilization), but must not touch dead status");
        assert_eq!(
            read_status(&store, id).await,
            "dead",
            "heartbeat self-reporting {hb_status:?} must NOT resurrect a dead host",
        );
    }

    // The heartbeat DID persist its other columns (proving we kept the
    // single per-heartbeat UPDATE and only pinned `status`): a dead host
    // that's still reachable keeps its capacity/utilization fresh.
    let cap: (i64, i32) = sqlx::query_as(
        r#"SELECT capacity_used_mib, running_sandboxes_count FROM hosts WHERE id = $1"#,
    )
    .bind(id.as_uuid())
    .fetch_one(store.pool())
    .await
    .expect("read host capacity");
    assert_eq!(cap.0, 1_024, "heartbeat still updated capacity_used_mib");
    assert_eq!(cap.1, 1, "heartbeat still updated running_sandboxes_count");

    // The ONLY way back: an explicit re-register (upsert_host, status=ready).
    store
        .upsert_host(host(id, &hostname, "http://10.0.0.1:9101"))
        .await
        .expect("re-register revives the dead row");
    assert_eq!(
        read_status(&store, id).await,
        "ready",
        "explicit re-register is the sanctioned dead -> ready path",
    );
}

/// A healthy host's heartbeats still freely move `ready <-> draining`
/// (the agent's self-reported preStop flag): the guard only pins `dead`.
#[tokio::test]
#[ignore = "requires live Postgres (ENGRAM_TEST_DATABASE_URL)"]
async fn heartbeat_still_moves_ready_and_draining() {
    let Some(store) = connect().await else { return };
    let id = HostId::new();
    let hostname = format!("hf-host-agent-test-{}", &id.to_string()[..8]);

    store
        .upsert_host(host(id, &hostname, "http://10.0.0.2:9101"))
        .await
        .expect("register host");

    store
        .touch_host_heartbeat(id, heartbeat(HostStatus::Draining))
        .await
        .expect("heartbeat");
    assert_eq!(read_status(&store, id).await, "draining");

    store
        .touch_host_heartbeat(id, heartbeat(HostStatus::Ready))
        .await
        .expect("heartbeat");
    assert_eq!(
        read_status(&store, id).await,
        "ready",
        "a host that finished its drain reports ready again — must be honored",
    );
}
