//! The conformance suite (ADR 0098 D4): every scenario runs against
//! BOTH `SimMetadataStore` and `PostgresStore` (a fresh template-cloned
//! database per test, on the same `ManualClock`), asserting identical
//! observable outcomes — returned values, error variants, resulting
//! visible state. This is the defense against SimMeta fidelity drift.
//!
//! **Process rule:** any PR that adds a `MetadataStore` method or
//! changes `PostgresStore` SQL semantics must extend this suite in the
//! same PR.
//!
//! The `pg_*` half is `#[ignore]`'d (needs `ENGRAM_TEST_DATABASE_URL`)
//! and runs in CI's Postgres-gated step alongside the coordinator's
//! live-PG lane.

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::{Clock, MetadataStore};
use engram_core::types::host::{HostCapacity, HostHeartbeat, HostRecord, HostStatus};
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::session_op::{EnqueueOutcome, OpKind, OpState};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::{HostId, MetaError, SessionId, SnapshotId};
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Ctx {
    meta: Arc<dyn MetadataStore>,
    clock: Arc<ManualClock>,
    /// Keeps the per-test PG database handle alive for the pg variant.
    _pg: Option<engram_testkit::pg::TestDb>,
}

fn sim_ctx() -> Ctx {
    let clock = ManualClock::new();
    let meta = SimMetadataStore::new(clock.clone(), Arc::new(SimEntropy::seeded(0xD4)));
    Ctx {
        meta,
        clock,
        _pg: None,
    }
}

async fn pg_ctx() -> Option<Ctx> {
    let db = engram_testkit::pg::fresh_db().await?;
    let clock = ManualClock::new();
    // Rebuild the store on the conformance clock (and a seeded entropy,
    // so id-minting paths are deterministic here too).
    let store = db
        .store
        .clone()
        .with_clock(clock.clone())
        .with_entropy(Arc::new(SimEntropy::seeded(0xD4)));
    Some(Ctx {
        meta: Arc::new(store),
        clock,
        _pg: Some(db),
    })
}

/// One scenario, two tests: `<name>::sim` (always) and `<name>::pg`
/// (`#[ignore]`'d, live Postgres).
macro_rules! conformance {
    ($name:ident, $scenario:path) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn sim() {
                let ctx = sim_ctx();
                $scenario(&ctx).await;
            }

            #[tokio::test]
            #[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
            async fn pg() {
                let Some(ctx) = pg_ctx().await else { return };
                $scenario(&ctx).await;
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn spec(image: &str) -> SessionSpec {
    SessionSpec {
        image: image.to_string(),
        mode: SessionMode::Agent,
    }
}

fn host_record(id: HostId, name: &str, now: chrono::DateTime<chrono::Utc>) -> HostRecord {
    HostRecord {
        id,
        hostname: name.to_string(),
        cloud_metadata: Default::default(),
        capacity: HostCapacity {
            total_gb: 100,
            used_gb: 0,
            total_mib: 32_768,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: now,
        host_addr: None,
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
    }
}

fn heartbeat_fixture() -> HostHeartbeat {
    HostHeartbeat {
        status: HostStatus::Ready,
        capacity: HostCapacity {
            total_gb: 100,
            used_gb: 0,
            total_mib: 32_768,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
    }
}

fn snapshot(
    id: SnapshotId,
    session: SessionId,
    at: chrono::DateTime<chrono::Utc>,
    recoverable: bool,
) -> SnapshotRecord {
    SnapshotRecord {
        id,
        session_id: Some(session),
        host_id: None,
        image_version: "conf:v1".to_string(),
        size_bytes: 0,
        created_at: at,
        last_accessed_at: at,
        disk_manifest: None,
        memory_manifest: None,
        recoverable,
        aux_bundles: Vec::new(),
        events_cursor: None,
        fc_snapshot_version: None,
    }
}

fn enabled_image(
    uri: &str,
    name: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> engram_core::types::registry::EnabledImage {
    engram_core::types::registry::EnabledImage {
        id: uuid::Uuid::nil(),
        image_uri: uri.to_string(),
        image_config: engram_core::types::image::ImageConfig {
            name: name.to_string(),
            resources: engram_core::types::image::ResourceHints {
                suggested_vcpus: Some(1),
                ..Default::default()
            },
            ..Default::default()
        },
        oci_defaults: Default::default(),
        manifest_digest: "sha256:conf".to_string(),
        disk_manifest: None,
        base_snapshot_id: None,
        base_snapshot_disk_manifest: None,
        base_snapshot_memory_manifest: None,
        last_refreshed_at: at,
        created_at: at,
        updated_at: None,
        soft_deleted_at: None,
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Transition legality: illegal edges are Conflict, transitions return
/// the PREVIOUS state, terminal states have no exits, missing rows are
/// NotFound.
async fn session_lifecycle(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta.create_session(spec("conf:lifecycle")).await.unwrap();
    let s = meta.get_session(id).await.unwrap();
    assert_eq!(s.status, SessionState::Pending);

    // Pending -> Active is illegal.
    let err = meta
        .transition_session(id, SessionState::Active)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)), "got {err:?}");

    // Pending -> Failed is legal and returns the previous state.
    let prev = meta
        .transition_session(id, SessionState::Failed)
        .await
        .unwrap();
    assert_eq!(prev, SessionState::Pending);

    // Terminal: no exits.
    let err = meta
        .transition_session(id, SessionState::Queued)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)));

    // Missing row.
    let err = meta
        .transition_session(SessionId::new(), SessionState::Failed)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::NotFound));
}

/// FIFO by queued_at; the queue drains oldest-first. Sessions enter the
/// queue via the REAL enqueue path (`reserve_and_persist_create` with no
/// candidates), which stamps `queue_origin` + `queued_at`.
///
/// FINDING (surfaced by this suite): a bare
/// `transition_session(_, Queued)` — legal per the FSM table — leaves
/// both columns NULL in PostgresStore, and `list_queued_sessions_fifo`
/// then fails to DECODE the row (`queue_origin: UnexpectedNullError`),
/// which would break the queue scanner. Production never enters Queued
/// that way today; flagged for a follow-up guard.
async fn queue_fifo(ctx: &Ctx) {
    let meta = &ctx.meta;
    let enqueue = |sid: SessionId| engram_core::traits::metadata::SessionCreateWriteSet {
        session_id: sid,
        spec: spec("conf:q"),
        mem_budget_mib: 1024,
        cpu_budget_vcpus: 1,
        sealed_secrets: None,
        capabilities: Vec::new(),
        integration_policy_json: None,
        runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(Vec::new(), None, None),
    };
    // No candidate hosts: both dispositions are Queued.
    let a = SessionId::new();
    let d = meta
        .reserve_and_persist_create(enqueue(a), &[], 0)
        .await
        .unwrap();
    assert!(matches!(
        d,
        engram_core::traits::metadata::CreateDisposition::Queued
    ));
    ctx.clock.advance(Duration::from_secs(5));
    let b = SessionId::new();
    let d = meta
        .reserve_and_persist_create(enqueue(b), &[], 0)
        .await
        .unwrap();
    assert!(matches!(
        d,
        engram_core::traits::metadata::CreateDisposition::Queued
    ));

    let q = meta.list_queued_sessions_fifo().await.unwrap();
    let ids: Vec<SessionId> = q.iter().map(|r| r.session.id).collect();
    assert_eq!(ids, vec![a, b], "FIFO must be oldest-first");
    assert!(q[0].queued_at < q[1].queued_at);
    assert!(q
        .iter()
        .all(|r| matches!(r.origin, engram_core::types::session::QueueOrigin::Create)));

    // The D4 finding, now guarded: a BARE transition_session(_, Queued)
    // (FSM-legal from Pending) stamps the queue columns, so the fifo
    // list decodes it instead of erroring on NULL queue_origin.
    ctx.clock.advance(Duration::from_secs(5));
    let c = meta.create_session(spec("conf:q")).await.unwrap();
    meta.transition_session(c, SessionState::Queued)
        .await
        .unwrap();
    let q = meta.list_queued_sessions_fifo().await.unwrap();
    let ids: Vec<SessionId> = q.iter().map(|r| r.session.id).collect();
    assert_eq!(ids, vec![a, b, c], "bare-transitioned session joins FIFO");
    assert!(matches!(
        q[2].origin,
        engram_core::types::session::QueueOrigin::Create
    ));
    assert!(
        q[1].queued_at < q[2].queued_at,
        "queued_at stamped at flip time"
    );
}

/// Dead-host lease: acquire, contest, claimant-guarded release, stale
/// takeover.
async fn dead_host_lease(ctx: &Ctx) {
    let meta = &ctx.meta;
    let host = HostId::new();
    let stale = Duration::from_secs(180);

    assert!(meta
        .try_acquire_dead_host_lease(host, "pod-a", stale)
        .await
        .unwrap());
    // Contested while live.
    assert!(!meta
        .try_acquire_dead_host_lease(host, "pod-b", stale)
        .await
        .unwrap());
    // Wrong-claimant release is a guarded no-op.
    meta.release_dead_host_lease(host, "pod-b").await.unwrap();
    assert!(!meta
        .try_acquire_dead_host_lease(host, "pod-b", stale)
        .await
        .unwrap());
    // Stale takeover.
    ctx.clock.advance(Duration::from_secs(181));
    assert!(meta
        .try_acquire_dead_host_lease(host, "pod-b", stale)
        .await
        .unwrap());
    // Right-claimant release frees it.
    meta.release_dead_host_lease(host, "pod-b").await.unwrap();
    assert!(meta
        .try_acquire_dead_host_lease(host, "pod-c", stale)
        .await
        .unwrap());
}

/// The op pipeline: claim epochs fence writes; requeue backoff gates
/// due-ness on the clock; reclaim bumps the epoch and fences out the
/// stalled claimant.
async fn ops_pipeline(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta.create_session(spec("conf:ops")).await.unwrap();

    // Enqueue+claim: first is Claimed, second is Queued behind it.
    let first = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::CreateBoot,
            serde_json::json!({}),
            None,
            "pod-a",
        )
        .await
        .unwrap();
    let EnqueueOutcome::Claimed(op1) = first else {
        panic!("first op must be Claimed, got {first:?}")
    };
    let second = meta
        .op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-a")
        .await
        .unwrap();
    assert!(matches!(second, EnqueueOutcome::Queued(_)));

    // One-running: claim_head is a no-op while op1 runs.
    assert!(meta.op_claim_head(sid, "pod-b").await.unwrap().is_none());

    // Fencing: a wrong-epoch finish silently fails.
    let epoch1 = op1.epoch.unwrap();
    assert!(!meta
        .op_finish(op1.id, epoch1 + 99, OpState::Done, None)
        .await
        .unwrap());
    assert!(meta
        .op_finish(op1.id, epoch1, OpState::Done, None)
        .await
        .unwrap());

    // Head-claim the queued op; epoch strictly increases.
    let op2 = meta
        .op_claim_head(sid, "pod-b")
        .await
        .unwrap()
        .expect("due head");
    let epoch2 = op2.epoch.unwrap();
    assert!(epoch2 > epoch1, "claim must bump the fencing epoch");

    // Requeue with backoff: not due until the clock passes not_before.
    assert!(meta
        .op_requeue_with_backoff(op2.id, epoch2, Duration::from_secs(30), "transient")
        .await
        .unwrap());
    assert!(meta.op_claim_head(sid, "pod-b").await.unwrap().is_none());
    assert!(meta.op_due_sessions().await.unwrap().is_empty());
    ctx.clock.advance(Duration::from_secs(31));
    assert_eq!(meta.op_due_sessions().await.unwrap(), vec![sid]);
    let op2b = meta
        .op_claim_head(sid, "pod-b")
        .await
        .unwrap()
        .expect("due after backoff");
    assert_eq!(op2b.id, op2.id);
    assert_eq!(op2b.attempts, 2, "attempts survive the requeue");
    assert_eq!(op2b.state, OpState::Running);

    // Reclaim: stale heartbeat hands the op to a new claimant at a new
    // epoch; the old claimant's fenced writes now miss.
    let epoch2b = op2b.epoch.unwrap();
    ctx.clock.advance(Duration::from_secs(120));
    let reclaimed = meta
        .op_reclaim_stale(Duration::from_secs(60), "pod-c")
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    let op2c = &reclaimed[0];
    assert_eq!(op2c.id, op2.id);
    assert!(op2c.epoch.unwrap() > epoch2b);
    assert_eq!(
        op2c.state,
        OpState::Running,
        "reclaim re-stamps, never re-queues"
    );
    assert!(
        !meta.op_heartbeat(op2.id, epoch2b).await.unwrap(),
        "stale claimant fenced out"
    );
    assert!(meta
        .op_heartbeat(op2.id, op2c.epoch.unwrap())
        .await
        .unwrap());
}

/// Idempotency keys dedupe ACTIVE ops only — a terminal keyed row does
/// not burn the key.
async fn ops_idempotency(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta.create_session(spec("conf:idem")).await.unwrap();
    let first = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("k1"),
            "pod-a",
        )
        .await
        .unwrap();
    let EnqueueOutcome::Claimed(op) = first else {
        panic!("claimed")
    };
    let dup = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("k1"),
            "pod-a",
        )
        .await
        .unwrap();
    assert!(matches!(dup, EnqueueOutcome::Duplicate));
    assert!(meta
        .op_finish(op.id, op.epoch.unwrap(), OpState::Done, None)
        .await
        .unwrap());
    let again = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("k1"),
            "pod-a",
        )
        .await
        .unwrap();
    assert!(
        matches!(again, EnqueueOutcome::Claimed(_)),
        "terminal keyed row must not burn the key: {again:?}"
    );
}

/// Fenced transition: epoch-mismatch is a SILENT None (never Conflict);
/// legality violations are Conflict (a caller bug, not a race).
async fn fenced_transition(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta.create_session(spec("conf:fence")).await.unwrap();
    // Claim an op to establish a nonzero epoch.
    let EnqueueOutcome::Claimed(op) = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::CreateBoot,
            serde_json::json!({}),
            None,
            "pod-a",
        )
        .await
        .unwrap()
    else {
        panic!("claimed")
    };
    let epoch = op.epoch.unwrap();

    assert!(meta
        .fenced_transition_session(sid, epoch + 1, SessionState::Failed)
        .await
        .unwrap()
        .is_none());
    let err = meta
        .fenced_transition_session(sid, epoch, SessionState::Active)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)));
    let prev = meta
        .fenced_transition_session(sid, epoch, SessionState::Failed)
        .await
        .unwrap();
    assert_eq!(prev, Some(SessionState::Pending));
}

/// Outbox: due-ness rides not_before on the shared clock; ack is
/// once-only; delivered rows can't be deleted as undelivered.
async fn outbox_flow(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta.create_session(spec("conf:outbox")).await.unwrap();
    let now = ctx.clock.now_utc();
    let row = engram_core::types::outbox::OutboxRow {
        prompt_id: "p-1".into(),
        session_id: sid,
        kind: engram_core::types::outbox::OutboxKind::Prompt,
        payload: serde_json::json!({"text": "hi"}),
        created_at: now,
        attempts: 0,
        not_before: now,
        delivered_at: None,
        acked_at: None,
    };
    meta.outbox_enqueue(&row).await.unwrap();
    // Idempotent re-enqueue.
    meta.outbox_enqueue(&row).await.unwrap();
    assert_eq!(meta.outbox_due_sessions().await.unwrap(), vec![sid]);
    let due = meta.outbox_next_due(sid).await.unwrap().expect("due row");
    assert_eq!(due.prompt_id, "p-1");

    meta.outbox_mark_delivered("p-1", Duration::from_secs(20))
        .await
        .unwrap();
    assert!(
        meta.outbox_next_due(sid).await.unwrap().is_none(),
        "ack window gates redelivery"
    );
    ctx.clock.advance(Duration::from_secs(21));
    let redue = meta
        .outbox_next_due(sid)
        .await
        .unwrap()
        .expect("redelivery due");
    assert_eq!(redue.attempts, 1);

    assert!(
        !meta.outbox_delete_undelivered("p-1").await.unwrap(),
        "delivered row not deletable"
    );
    assert!(meta.outbox_ack("p-1").await.unwrap());
    assert!(!meta.outbox_ack("p-1").await.unwrap(), "ack is once-only");
    assert!(meta.outbox_next_due(sid).await.unwrap().is_none());
}

/// Snapshot durable head: monotonic by created_at over recoverable
/// rows; demoting the head re-points to the newest recoverable sibling.
async fn snapshot_durable_head(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta.create_session(spec("conf:snap")).await.unwrap();
    let t0 = ctx.clock.now_utc();
    let s1 = SnapshotId::new();
    assert!(meta
        .record_snapshot(snapshot(s1, sid, t0, true))
        .await
        .unwrap());
    assert_eq!(meta.durable_head_snapshot(sid).await.unwrap(), Some(s1));

    ctx.clock.advance(Duration::from_secs(10));
    let t1 = ctx.clock.now_utc();
    let s2 = SnapshotId::new();
    assert!(meta
        .record_snapshot(snapshot(s2, sid, t1, true))
        .await
        .unwrap());
    assert_eq!(meta.durable_head_snapshot(sid).await.unwrap(), Some(s2));

    // An OLDER recoverable row must not regress the head.
    let s0 = SnapshotId::new();
    assert!(meta
        .record_snapshot(snapshot(s0, sid, t0 - chrono::Duration::seconds(5), true))
        .await
        .unwrap());
    assert_eq!(meta.durable_head_snapshot(sid).await.unwrap(), Some(s2));

    // Demote the head: re-points to the newest recoverable sibling.
    assert!(!meta
        .record_snapshot(snapshot(s2, sid, t1, false))
        .await
        .unwrap());
    assert_eq!(meta.durable_head_snapshot(sid).await.unwrap(), Some(s1));
}

/// GC candidates: first_seen_at is sticky across re-upserts; the
/// expiry cutoff keys on it; delete removes.
async fn gc_candidates(ctx: &Ctx) {
    let meta = &ctx.meta;
    let h = [7u8; 32];
    assert_eq!(meta.count_gc_candidates().await.unwrap(), 0);
    meta.upsert_chunk_gc_candidate(h).await.unwrap();
    ctx.clock.advance(Duration::from_secs(100));
    meta.upsert_chunk_gc_candidate(h).await.unwrap(); // sticky first_seen
    assert_eq!(
        meta.count_gc_candidates().await.unwrap(),
        1,
        "re-upsert of the same hash does not double-count"
    );

    let now = ctx.clock.now_utc();
    // Cutoff after first_seen: expired (proves last_seen didn't reset it).
    let expired = meta
        .list_expired_gc_candidates(now - chrono::Duration::seconds(50), 10)
        .await
        .unwrap();
    assert_eq!(expired, vec![h]);
    // Cutoff before first_seen: not expired.
    let not_yet = meta
        .list_expired_gc_candidates(now - chrono::Duration::seconds(150), 10)
        .await
        .unwrap();
    assert!(not_yet.is_empty());
    meta.delete_gc_candidates(&[h]).await.unwrap();
    assert!(meta
        .list_expired_gc_candidates(now, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        meta.count_gc_candidates().await.unwrap(),
        0,
        "delete drops the candidate from the count"
    );
}

/// Host staleness: cordoned hosts get the 10x-lenient bar; dead is
/// sticky against heartbeats; mark_host_dead orphans with honest prevs
/// and is idempotent.
async fn host_lifecycle(ctx: &Ctx) {
    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    let h1 = HostId::new();
    let h2 = HostId::new();
    meta.upsert_host(host_record(h1, "conf-h1", now))
        .await
        .unwrap();
    meta.upsert_host(host_record(h2, "conf-h2", now))
        .await
        .unwrap();
    meta.set_host_cordoned(h2, true).await.unwrap();

    assert!(meta.list_stale_hosts(30).await.unwrap().is_empty());
    ctx.clock.advance(Duration::from_secs(31));
    let stale: Vec<HostId> = meta
        .list_stale_hosts(30)
        .await
        .unwrap()
        .iter()
        .map(|h| h.id)
        .collect();
    assert_eq!(stale, vec![h1], "cordoned host shielded at 1x staleness");
    ctx.clock.advance(Duration::from_secs(270));
    assert_eq!(
        meta.list_stale_hosts(30).await.unwrap().len(),
        2,
        "cordon shield expires at 10x"
    );

    // Orphaning: honest prev states, idempotent.
    let sid = meta.create_session(spec("conf:orphan")).await.unwrap();
    meta.assign_session_host(sid, Some(h1)).await.unwrap();
    let affected = meta.mark_host_dead_and_orphan_sessions(h1).await.unwrap();
    assert_eq!(affected, vec![(sid, SessionState::Pending)]);
    assert_eq!(
        meta.get_session(sid).await.unwrap().status,
        SessionState::HostLost
    );
    assert!(meta
        .mark_host_dead_and_orphan_sessions(h1)
        .await
        .unwrap()
        .is_empty());

    // Dead is sticky against a Ready-claiming heartbeat.
    let hb = HostHeartbeat {
        status: HostStatus::Ready,
        capacity: host_record(h1, "conf-h1", now).capacity,
        utilization: Default::default(),
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
    };
    meta.touch_host_heartbeat(h1, hb).await.unwrap();
    assert_eq!(meta.host_status(h1).await.unwrap(), Some(HostStatus::Dead));
}

/// placement_no_fit_details: per-host fit verdicts with the shared
/// reason labels, matching the pick's own eligibility + reservation
/// arithmetic on both stores.
async fn placement_no_fit(ctx: &Ctx) {
    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    let ready = HostId::new();
    let cordoned = HostId::new();
    let unknown = HostId::new();
    // Registration alone leaves a host UNMEASURED (utilization and
    // vcpus are heartbeat-only columns — upsert_host never writes
    // them); the heartbeat is what makes it placeable. This split is
    // itself conformance-tested: SimMeta originally clobbered the
    // heartbeat columns from the record and this scenario caught it.
    meta.upsert_host(host_record(ready, "conf-fit-ready", now))
        .await
        .unwrap();
    let unmeasured = meta
        .placement_no_fit_details(&[ready], 4096, 2)
        .await
        .unwrap();
    assert_eq!(unmeasured[0].reason, "unmeasured");
    let mut hb = heartbeat_fixture();
    hb.utilization =
        serde_json::from_value(serde_json::json!({"allocatable_mib": 8192})).expect("utilization");
    hb.total_vcpus = 4;
    meta.touch_host_heartbeat(ready, hb).await.unwrap();
    meta.upsert_host(host_record(cordoned, "conf-fit-cord", now))
        .await
        .unwrap();
    meta.set_host_cordoned(cordoned, true).await.unwrap();

    let details = meta
        .placement_no_fit_details(&[ready, cordoned, unknown], 4096, 2)
        .await
        .unwrap();
    assert_eq!(details.len(), 3);
    let by_host: std::collections::BTreeMap<_, _> =
        details.iter().map(|d| (d.host_id, d)).collect();
    assert_eq!(by_host[&ready].reason, "fits_now");
    assert_eq!(by_host[&ready].free_mib, 8192);
    assert_eq!(by_host[&cordoned].reason, "not_lockable");
    assert_eq!(by_host[&unknown].reason, "not_lockable");

    // Over-budget asks classify against the binding dimension.
    let details = meta
        .placement_no_fit_details(&[ready], 16_384, 2)
        .await
        .unwrap();
    assert_eq!(details[0].reason, "ram_full");
    // CPU budgets are overcommitted (host_cpu_budget = vcpus x factor,
    // default 4): 4 vcpus => budget 16, so an ask of 17 is cpu_full but
    // 8 still fits. Both stores must share the overcommit arithmetic
    // (the conformance suite caught SimMeta using raw vcpus).
    let details = meta
        .placement_no_fit_details(&[ready], 4096, 8)
        .await
        .unwrap();
    assert_eq!(details[0].reason, "fits_now");
    let details = meta
        .placement_no_fit_details(&[ready], 4096, 17)
        .await
        .unwrap();
    assert_eq!(details[0].reason, "cpu_full");
}

conformance!(t_session_lifecycle, super::session_lifecycle);
conformance!(t_queue_fifo, super::queue_fifo);
conformance!(t_dead_host_lease, super::dead_host_lease);
conformance!(t_ops_pipeline, super::ops_pipeline);
conformance!(t_ops_idempotency, super::ops_idempotency);
conformance!(t_fenced_transition, super::fenced_transition);
conformance!(t_outbox_flow, super::outbox_flow);
conformance!(t_snapshot_durable_head, super::snapshot_durable_head);
conformance!(t_gc_candidates, super::gc_candidates);
conformance!(t_host_lifecycle, super::host_lifecycle);

/// Issue #722: the reservation predicate. A stale `pending` WITH a live
/// create_boot op still holds its budget (visible through
/// placement_no_fit_details' free_mib); a stale op-less pending is
/// written off — and the reclaim sweep fails those, so they can never
/// boot later and over-pack the host.
async fn stale_pending_reservation(ctx: &Ctx) {
    use engram_core::types::session_op::{EnqueueOutcome, OpKind};
    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    let host = HostId::new();
    meta.upsert_host(host_record(host, "conf-722", now))
        .await
        .unwrap();
    let mut hb = heartbeat_fixture();
    hb.utilization =
        serde_json::from_value(serde_json::json!({"allocatable_mib": 8192})).expect("utilization");
    meta.touch_host_heartbeat(host, hb).await.unwrap();

    // A placed pending with a queued create_boot op.
    let with_op = SessionId::new();
    let ws = |sid: SessionId| engram_core::traits::metadata::SessionCreateWriteSet {
        session_id: sid,
        spec: spec("conf:722"),
        mem_budget_mib: 2048,
        cpu_budget_vcpus: 1,
        sealed_secrets: None,
        capabilities: Vec::new(),
        integration_policy_json: None,
        runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(Vec::new(), None, None),
    };
    let d = meta
        .reserve_and_persist_create(ws(with_op), &[host], 0)
        .await
        .unwrap();
    assert!(matches!(
        d,
        engram_core::traits::metadata::CreateDisposition::Placed(_)
    ));
    let outcome = meta
        .op_enqueue_and_claim(
            with_op,
            OpKind::CreateBoot,
            serde_json::json!({}),
            None,
            "pod",
        )
        .await
        .unwrap();
    let EnqueueOutcome::Claimed(op) = outcome else {
        panic!("claimed")
    };
    // Requeue it (a retrying boot) so the op is live-but-queued.
    assert!(meta
        .op_requeue_with_backoff(op.id, op.epoch.unwrap(), Duration::from_secs(600), "slow")
        .await
        .unwrap());

    // A placed pending with NO op (the orphan).
    let orphan = SessionId::new();
    let d = meta
        .reserve_and_persist_create(ws(orphan), &[host], 0)
        .await
        .unwrap();
    assert!(matches!(
        d,
        engram_core::traits::metadata::CreateDisposition::Placed(_)
    ));

    // Fresh: both reserve — 8192 - 2*2048 = 4096 free.
    let details = meta.placement_no_fit_details(&[host], 1, 1).await.unwrap();
    assert_eq!(details[0].free_mib, 4096);

    // Cross the 10-minute horizon: the live-op pending still counts,
    // the op-less orphan is written off — 8192 - 2048 = 6144 free.
    ctx.clock.advance(Duration::from_secs(11 * 60));
    let details = meta.placement_no_fit_details(&[host], 1, 1).await.unwrap();
    assert_eq!(
        details[0].free_mib, 6144,
        "stale live-op pending must keep its reservation; op-less orphan must not"
    );
}

/// ADR 0080 cheap-edit path (`update_enabled_image_config`): an in-place
/// config replace on a LIVE row is visible via `get_enabled_image`;
/// `updated_at` is stamped; an unknown uri and a soft-deleted row both
/// surface `NotFound` (the `soft_deleted_at IS NULL` guard — editing a
/// disabled image is a re-enable's job).
async fn enabled_image_config_update(ctx: &Ctx) {
    let meta = &ctx.meta;
    let uri = "conf.local/img:warm";
    let at = ctx.clock.now_utc();
    meta.upsert_enabled_image(enabled_image(uri, "before", at))
        .await
        .unwrap();

    // Unknown uri → NotFound.
    let edited = enabled_image(uri, "after", at).image_config;
    let err = meta
        .update_enabled_image_config("conf.local/nope:warm", &edited)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::NotFound), "got {err:?}");

    // Live row → in-place replace, visible through the live-only read.
    ctx.clock.advance(Duration::from_secs(10));
    meta.update_enabled_image_config(uri, &edited)
        .await
        .unwrap();
    let row = meta
        .get_enabled_image(uri)
        .await
        .unwrap()
        .expect("row still enabled");
    assert_eq!(row.image_config.name, "after", "config replaced in place");
    assert!(row.updated_at.is_some(), "updated_at stamped on cheap edit");

    // Soft-deleted row → NotFound (the UPDATE's soft_deleted_at guard
    // matches 0 rows).
    meta.soft_delete_enabled_image(uri).await.unwrap();
    let err = meta
        .update_enabled_image_config(uri, &edited)
        .await
        .unwrap_err();
    assert!(
        matches!(err, MetaError::NotFound),
        "editing a disabled image must be NotFound: {err:?}"
    );
}

/// The enable/capture read surface over a store with no such jobs:
/// `live_enable_work_by_host` is the empty map, while `list_prestaging_refs`
/// and `capture_assignments_for_host` are empty — and registering or
/// heartbeating a host fabricates none of them. (The sim models no
/// enable_jobs/capture_jobs table; a fresh PG DB has no such rows — both
/// stores agree on the empty case, the only one conformance exercises.)
async fn live_enable_work_empty(ctx: &Ctx) {
    let meta = &ctx.meta;
    assert!(meta
        .live_enable_work_by_host(Duration::from_secs(45))
        .await
        .unwrap()
        .is_empty());
    assert!(meta.list_prestaging_refs().await.unwrap().is_empty());
    let host = HostId::new();
    let now = ctx.clock.now_utc();
    assert!(meta
        .capture_assignments_for_host(host)
        .await
        .unwrap()
        .is_empty());
    meta.upsert_host(host_record(host, "conf-lew", now))
        .await
        .unwrap();
    meta.touch_host_heartbeat(host, heartbeat_fixture())
        .await
        .unwrap();
    assert!(
        meta.live_enable_work_by_host(Duration::from_secs(45))
            .await
            .unwrap()
            .is_empty(),
        "a registered+heartbeating host with no jobs still has no live work"
    );
    assert!(
        meta.list_prestaging_refs().await.unwrap().is_empty(),
        "no enable jobs → nothing to prestage"
    );
    assert!(
        meta.capture_assignments_for_host(host)
            .await
            .unwrap()
            .is_empty(),
        "no capture jobs → nothing assigned to the host"
    );
}

/// `snapshot_totals`: fleet-wide count + Σ size_bytes over all snapshot
/// rows. Empty store → zeros; recording rows advances both aggregates.
async fn snapshot_totals_aggregate(ctx: &Ctx) {
    let meta = &ctx.meta;
    assert_eq!(
        meta.snapshot_totals().await.unwrap(),
        engram_core::traits::SnapshotTotals::default(),
        "empty store has zero snapshots"
    );
    let sid = meta.create_session(spec("conf:totals")).await.unwrap();
    let now = ctx.clock.now_utc();
    let mut s1 = snapshot(SnapshotId::new(), sid, now, true);
    s1.size_bytes = 1000;
    let mut s2 = snapshot(SnapshotId::new(), sid, now, true);
    s2.size_bytes = 2500;
    meta.record_snapshot(s1).await.unwrap();
    meta.record_snapshot(s2).await.unwrap();
    let totals = meta.snapshot_totals().await.unwrap();
    assert_eq!(totals.count, 2);
    assert_eq!(totals.total_bytes, 3500);
}

conformance!(t_placement_no_fit, super::placement_no_fit);
conformance!(
    t_enabled_image_config_update,
    super::enabled_image_config_update
);
conformance!(t_live_enable_work_empty, super::live_enable_work_empty);
conformance!(t_snapshot_totals, super::snapshot_totals_aggregate);
conformance!(
    t_stale_pending_reservation,
    super::stale_pending_reservation
);
