//! ADR 0092 hardening: live-Postgres regression tests for the reverse
//! ownership oracle `session_owning_sandbox` (ADR 0090's "does ANY
//! session own sandbox S on host H?" — the teardown reconciler's
//! unbound-sandbox gate).
//!
//! Before the fix the query was malformed (a dangling `AND status IN
//! ('pending',...` fragment AFTER `LIMIT 1`): Postgres rejected it on
//! EVERY call, `sandbox_owner` returned 500, and the host-agent's
//! `Err(_) => assume owned` posture protected every unbound orphan VM
//! from the reap forever — the failed-create leak that repeatedly pinned
//! ~20 GiB of guest memory on the dev VM during the WS0 campaign. These
//! tests exercise the REAL Postgres impl; the first one fails against
//! the malformed query (MetaError, not Ok).
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. CI wires this into the Postgres-gated
//! ignored lane alongside `binding_cas_live_pg`.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use engram_core::types::BindingDisposition;
use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::{HostId, SandboxId};

async fn pg() -> Option<Arc<dyn MetadataStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
}

/// Insert a `hosts` row so `sessions_host_id_fkey` is satisfied.
/// Hostname carries the host_id suffix to dodge `ON CONFLICT (hostname)`
/// collisions when a test seeds more than one host.
async fn ensure_host(meta: &Arc<dyn MetadataStore>, host_id: HostId) {
    use engram_core::types::host::HostRecord;
    use engram_core::types::{HostCapacity, HostMetadata, HostStatus};
    meta.upsert_host(HostRecord {
        id: host_id,
        hostname: format!("sandbox-owner-{host_id}"),
        cloud_metadata: HostMetadata::default(),
        capacity: HostCapacity {
            total_gb: 100,
            used_gb: 10,
            total_mib: 65_536,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
        host_addr: None,
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
        capabilities: engram_core::types::host::HostCapabilities::default(),
    })
    .await
    .expect("upsert_host");
}

/// A session bound to (host, sandbox) in the given state.
async fn seed_bound(
    meta: &Arc<dyn MetadataStore>,
    host: HostId,
    sandbox: SandboxId,
) -> engram_core::SessionId {
    let id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:sandbox-owner-test".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    meta.assign_session_host(id, Some(host))
        .await
        .expect("bind host");
    // 0108: bind via the production fused path (Created), never on a
    // Pending row.
    meta.transition_session_created(id, sandbox)
        .await
        .expect("bind sandbox (fused Pending → Created)");
    id
}

/// The happy path IS the regression test for the malformed query: before
/// the fix this call returned `Err(MetaError::Database)` on every
/// invocation (Postgres syntax error), never `Ok`.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn owner_query_executes_and_finds_active_binding() {
    let Some(meta) = pg().await else { return };
    let host = HostId::new();
    ensure_host(&meta, host).await;
    let sandbox = SandboxId::new();
    let id = seed_bound(&meta, host, sandbox).await;

    let owner = meta
        .session_owning_sandbox(host, sandbox)
        .await
        .expect("query must execute (was a Postgres syntax error before the fix)");
    assert_eq!(owner, Some(id), "pending session owns its bound sandbox");

    // Wrong host → no owner (the host_id predicate is live).
    let other = HostId::new();
    ensure_host(&meta, other).await;
    let none = meta
        .session_owning_sandbox(other, sandbox)
        .await
        .expect("query executes");
    assert_eq!(none, None, "binding is per-host");
}

/// A terminal row owns nothing, even with `sandbox_id` still set — the
/// failed-create shape (boot pipeline flips Failed without clearing the
/// binding; the reaper must be allowed to strike).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn owner_query_ignores_terminal_rows() {
    let Some(meta) = pg().await else { return };
    let host = HostId::new();
    ensure_host(&meta, host).await;
    let sandbox = SandboxId::new();
    let id = seed_bound(&meta, host, sandbox).await;

    meta.transition_session(id, SessionState::Failed, BindingDisposition::Detach)
        .await
        .expect("pending->failed (the failed-create disposition)");

    let owner = meta
        .session_owning_sandbox(host, sandbox)
        .await
        .expect("query executes");
    assert_eq!(
        owner, None,
        "a Failed row must not shield its orphan from the reap"
    );
}
