//! Issue #211: live-Postgres regression tests for the guarded binding
//! writers.
//!
//! Before this fix, `assign_session_host`, `assign_session_sandbox`, and
//! `rebind_session` were blind `WHERE id = $1` UPDATEs with no state or
//! expected-value guard. A racing actor could bind a live sandbox onto a
//! row that had concurrently gone terminal (the terminate-races-resume
//! interleaving) — the ownership oracle matches `sandbox_id` only and
//! ignores terminal status, so the bound-to-terminal-row VM was protected
//! from the orphan reap and leaked forever. A reconcile strike could also
//! null a freshly-landed migration rebind.
//!
//! These tests exercise the `*_guarded` CAS overrides against the REAL
//! Postgres schema + legality-enforcing `transition_session` (not a mock
//! whose default impl is only get-then-set). They FAIL without the
//! Postgres CAS override (the blind methods accept every write) and pass
//! with it.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. CI wires this into the Postgres-gated
//! ignored lane alongside `eviction_live_pg`.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use engram_core::types::BindingDisposition;
use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::{HostId, MetaError, SandboxId, SessionId};

async fn pg() -> Option<Arc<dyn MetadataStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
}

/// Insert a `hosts` row so PG's `sessions_host_id_fkey` is satisfied
/// when a test binds a host. Hostname carries the host_id suffix to dodge
/// the `ON CONFLICT (hostname)` collision other suites hit.
async fn ensure_host(meta: &Arc<dyn MetadataStore>, host_id: HostId) {
    use engram_core::types::host::HostRecord;
    use engram_core::types::{HostCapacity, HostMetadata, HostStatus};
    meta.upsert_host(HostRecord {
        id: host_id,
        hostname: format!("binding-cas-{host_id}"),
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

/// Walk a fresh session to `Idle` with `sandbox_id` cleared — the exact
/// shape `resume_from_idle` dispatches on (the resume op's claim held,
/// it restores + binds).
async fn seed_idle_unbound(meta: &Arc<dyn MetadataStore>) -> SessionId {
    let id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:binding-cas-test".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    let sandbox = SandboxId::new();
    // 0108: bind via the production fused path, never on a Pending row.
    meta.transition_session_created(id, sandbox)
        .await
        .expect("pending->created");
    meta.transition_session(id, SessionState::Active, BindingDisposition::Retain)
        .await
        .expect("created->active");
    meta.transition_session(id, SessionState::Idle, BindingDisposition::Detach)
        .await
        .expect("active->idle");
    meta.assign_session_sandbox(id, None)
        .await
        .expect("clear sandbox (evict_local / resume-dispatch shape)");
    id
}

/// Interleaving 1 (terminate races resume): a `DELETE /sessions/:id`
/// flips `Idle → Completed` while the resume's `restore_for_session` is
/// in flight. The resume's `bind_resumed_session` then tries to bind the
/// freshly-restored sandbox. The guarded rebind MUST reject it with
/// `Conflict` (allowed_states = [Idle], expected sandbox = None), so the
/// caller destroys the VM instead of pinning it to the terminal row.
///
/// Without the CAS override this would SUCCEED onto the Completed row.
#[tokio::test]
#[ignore]
async fn guarded_rebind_rejects_bind_onto_terminated_row() {
    let Some(meta) = pg().await else { return };
    let id = seed_idle_unbound(&meta).await;

    // Terminate wins the race: Idle -> Completed (legal terminal edge).
    let prev = meta
        .transition_session(id, SessionState::Completed, BindingDisposition::Detach)
        .await
        .expect("idle->completed (terminate)");
    assert_eq!(prev, SessionState::Idle);

    // Resume's bind lands AFTER the terminate. It expects the row still
    // Idle + unbound — the guard must refuse the Completed row.
    let new_sandbox = SandboxId::new();
    let host = HostId::new();
    ensure_host(&meta, host).await;
    let err = meta
        .rebind_session_guarded(id, host, new_sandbox, Some(None), &[SessionState::Idle])
        .await
        .expect_err("rebind onto a Completed row MUST be rejected, not silently bound");
    assert!(
        matches!(err, MetaError::Conflict(_)),
        "expected Conflict, got {err:?}",
    );

    // The terminal row must NOT have been bound to the live VM — that is
    // exactly the leak that defeats the orphan reap.
    let row = meta.get_session(id).await.expect("get_session");
    assert_eq!(row.status, SessionState::Completed, "row stayed terminal");
    assert_eq!(
        row.sandbox_id, None,
        "no sandbox may be bound to the terminated row (orphan-reap defeat)",
    );
}

/// The happy path the guard must still admit: the row is the `Idle`,
/// unbound row the resume dispatched on, so the guarded rebind binds
/// host + sandbox atomically and the session is routable.
#[tokio::test]
#[ignore]
async fn guarded_rebind_admits_the_idle_unbound_row() {
    let Some(meta) = pg().await else { return };
    let id = seed_idle_unbound(&meta).await;

    let new_sandbox = SandboxId::new();
    let host = HostId::new();
    ensure_host(&meta, host).await;
    meta.rebind_session_guarded(id, host, new_sandbox, Some(None), &[SessionState::Idle])
        .await
        .expect("rebind onto the Idle/unbound row must succeed");

    let row = meta.get_session(id).await.expect("get_session");
    assert_eq!(row.sandbox_id, Some(new_sandbox), "sandbox bound");
    assert_eq!(row.host_id, Some(host), "host bound (atomic with sandbox)");
}

/// Interleaving 3 (reconcile clears a fresh rebind): the strike-out reads
/// sandbox `A`, but a live migration rebinds the row to sandbox `B`
/// before reconcile's clear runs. The clear is a CAS on the EXACT struck
/// sandbox `A` — it must NOT null the healthy `B` binding; it returns
/// `Conflict` so `flip_missing` aborts the flip.
#[tokio::test]
#[ignore]
async fn guarded_clear_does_not_null_a_fresh_rebind() {
    let Some(meta) = pg().await else { return };
    let id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:binding-cas-test".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    let sandbox_a = SandboxId::new();
    // 0108: the production fused Pending→Created bind.
    meta.transition_session_created(id, sandbox_a)
        .await
        .expect("bind A");

    // A migration rebinds A -> B (the fresh, healthy binding).
    let sandbox_b = SandboxId::new();
    meta.assign_session_sandbox(id, Some(sandbox_b))
        .await
        .expect("rebind to B");

    // Reconcile struck out on A; its guarded clear must lose cleanly.
    let err = meta
        .assign_session_sandbox_guarded(id, None, Some(Some(sandbox_a)), &[])
        .await
        .expect_err("CAS clear on the stale sandbox A MUST be rejected");
    assert!(
        matches!(err, MetaError::Conflict(_)),
        "expected Conflict, got {err:?}",
    );

    let row = meta.get_session(id).await.expect("get_session");
    assert_eq!(
        row.sandbox_id,
        Some(sandbox_b),
        "the fresh rebind to B must survive a stale reconcile clear",
    );
}

/// And the guarded clear DOES fire when the binding is the one it read —
/// the normal reconcile / delete teardown.
#[tokio::test]
#[ignore]
async fn guarded_clear_fires_on_the_matching_sandbox() {
    let Some(meta) = pg().await else { return };
    let id = meta
        .create_session(SessionSpec {
            image: "localhost:5001/demo:binding-cas-test".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create");
    let sandbox = SandboxId::new();
    // 0108: the production fused Pending→Created bind.
    meta.transition_session_created(id, sandbox)
        .await
        .expect("bind");

    meta.assign_session_sandbox_guarded(id, None, Some(Some(sandbox)), &[])
        .await
        .expect("CAS clear on the matching sandbox must succeed");

    let row = meta.get_session(id).await.expect("get_session");
    assert_eq!(row.sandbox_id, None, "binding cleared");
}
