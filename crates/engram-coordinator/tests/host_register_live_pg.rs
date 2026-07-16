//! Live-Postgres regression for idempotent host registration (the
//! prod jpjdb incident): host identity is the STABLE `id` (persisted
//! work_dir / node-name-derived — ADR 0044 K2), while `hostname` is
//! the K8s POD name and changes on every fleet roll. The legacy upsert
//! conflicted on `hostname`, so a successor pod re-registering the
//! same host id under its new pod name hit `hosts_pkey` with a
//! duplicate-key 500 and looped every 30 s — leaving register-carried
//! fields (`host_addr`!) permanently stale. The upsert now conflicts
//! on `(id)` and migration 0059 drops the hostname uniqueness.
//!
//! `#[ignore]`'d by default; requires Postgres at `ENGRAM_TEST_DATABASE_URL`.
//! Run:
//! ```bash
//! just db-up
//! ENGRAM_TEST_DATABASE_URL=postgres://engram:engram@localhost:5435/engram \
//!     cargo test -p engram-coordinator --test host_register_live_pg -- --ignored
//! ```

use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::host::{HostCapacity, HostMetadata, HostRecord, HostStatus};
use engram_core::types::HostId;

async fn connect() -> Option<Arc<dyn MetadataStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
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
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
        capabilities: engram_core::types::host::HostCapabilities::default(),
    }
}

/// Same host id, new pod hostname (the fleet-roll successor): the
/// second register must SUCCEED and update hostname + host_addr —
/// not 500 on hosts_pkey.
#[tokio::test]
#[ignore = "requires live Postgres (ENGRAM_TEST_DATABASE_URL)"]
async fn re_register_same_id_new_pod_name_updates_in_place() {
    let Some(meta) = connect().await else { return };
    let id = HostId::new();
    let pod1 = format!("hf-host-agent-test-{}a", &id.to_string()[..8]);
    let pod2 = format!("hf-host-agent-test-{}b", &id.to_string()[..8]);

    meta.upsert_host(host(id, &pod1, "http://10.0.0.1:9101"))
        .await
        .expect("first register");
    meta.upsert_host(host(id, &pod2, "http://10.0.0.2:9101"))
        .await
        .expect("successor pod re-register with the same host id must not 500");

    let rows = meta.list_active_hosts().await.expect("list hosts");
    let row = rows
        .iter()
        .find(|h| h.id == id)
        .expect("host row present once");
    assert_eq!(row.hostname, pod2, "hostname follows the live pod");
    assert_eq!(
        row.host_addr.as_deref(),
        Some("http://10.0.0.2:9101"),
        "host_addr updated by re-register (the register-only-stale gap)",
    );
    assert_eq!(
        rows.iter().filter(|h| h.id == id).count(),
        1,
        "exactly one row per host id",
    );
}
