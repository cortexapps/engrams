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
use engram_core::types::capture_job::{
    CaptureJobReport, CaptureJobStage, CaptureTerminalReport, NewCaptureJob,
};
use engram_core::types::host::{HostCapacity, HostHeartbeat, HostRecord, HostStatus};
use engram_core::types::image::ImageConfig;
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

fn new_capture(enable_job_id: uuid::Uuid) -> NewCaptureJob {
    NewCaptureJob {
        enable_job_id,
        image_uri: format!("conf:capture:{enable_job_id}"),
        manifest_digest: "sha256:conf".into(),
        disk_manifest: format!("{}@v1", uuid::Uuid::nil()),
        image_config: ImageConfig::default(),
        oci_defaults: Default::default(),
        mem_budget_mib: 512,
        cpu_budget_vcpus: 1,
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

async fn enable_job_claim_boundary(ctx: &Ctx) {
    let config = ImageConfig::default();
    let first = ctx
        .meta
        .create_or_get_enable_job("conf:enable:first", Some("sha256:first"), &config)
        .await
        .unwrap();
    let same = ctx
        .meta
        .create_or_get_enable_job("conf:enable:first", Some("sha256:changed"), &config)
        .await
        .unwrap();
    assert_eq!(same.id, first.id, "active create-or-get is idempotent");

    ctx.clock.advance(Duration::from_secs(1));
    let second = ctx
        .meta
        .create_or_get_enable_job("conf:enable:second", None, &config)
        .await
        .unwrap();
    let claimed = ctx.meta.claim_enable_jobs("pod-a", 30, 1).await.unwrap();
    assert_eq!(claimed.len(), 1, "claim_limit is respected");
    assert_eq!(
        claimed[0].id, first.id,
        "oldest active job is claimed first"
    );

    let by_b = ctx.meta.claim_enable_jobs("pod-b", 30, 1).await.unwrap();
    assert_eq!(by_b.len(), 1);
    assert_eq!(by_b[0].id, second.id, "held lease is skipped");
    assert!(ctx
        .meta
        .claim_enable_jobs("pod-b", 30, 10)
        .await
        .unwrap()
        .is_empty());

    ctx.clock.advance(Duration::from_secs(31));
    let reclaimed = ctx.meta.claim_enable_jobs("pod-b", 30, 1).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(
        reclaimed[0].id, first.id,
        "expired oldest lease is reclaimable"
    );
}

async fn capture_job_scan_boundary(ctx: &Ctx) {
    let now = ctx.clock.now_utc();
    let host = HostId::from(uuid::Uuid::from_u128(0xc001));
    ctx.meta
        .upsert_host(host_record(host, "capture-host", now))
        .await
        .unwrap();

    // Real enable-job parents: `capture_jobs.enable_job_id` is an FK in
    // PG, so a made-up uuid is rejected there (and prod never inserts a
    // capture job without its enable row).
    let config = ImageConfig::default();
    let waiting_parent = ctx
        .meta
        .create_or_get_enable_job("conf:capture:waiting", None, &config)
        .await
        .unwrap()
        .id;
    let placed_parent = ctx
        .meta
        .create_or_get_enable_job("conf:capture:placed", None, &config)
        .await
        .unwrap()
        .id;
    let terminal_parent = ctx
        .meta
        .create_or_get_enable_job("conf:capture:terminal", None, &config)
        .await
        .unwrap()
        .id;

    let waiting = ctx
        .meta
        .insert_capture_job(new_capture(waiting_parent))
        .await
        .unwrap();
    let placed = ctx
        .meta
        .insert_capture_job(new_capture(placed_parent))
        .await
        .unwrap();
    let placed = ctx
        .meta
        .place_capture_job(placed.id, &[host])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(placed.host_id, Some(host));

    let terminal = ctx
        .meta
        .insert_capture_job(new_capture(terminal_parent))
        .await
        .unwrap();
    let terminal = ctx
        .meta
        .place_capture_job(terminal.id, &[host])
        .await
        .unwrap()
        .unwrap();
    assert!(ctx
        .meta
        .record_capture_job_report(&CaptureJobReport {
            job_id: terminal.id,
            epoch: terminal.epoch,
            stage: CaptureJobStage::Done,
            progress: None,
            fc_snapshot_version: None,
            terminal: Some(CaptureTerminalReport::Done {
                result_bincode: Vec::new(),
            }),
        })
        .await
        .unwrap());

    let waiting_rows = ctx.meta.list_waiting_capture_jobs().await.unwrap();
    assert_eq!(waiting_rows.len(), 1);
    assert_eq!(
        waiting_rows[0].id, waiting.id,
        "only unplaced non-terminal rows wait"
    );

    let budgets = [(CaptureJobStage::Assigned, Duration::from_secs(30))];
    assert!(ctx
        .meta
        .expire_capture_job_stages(&budgets)
        .await
        .unwrap()
        .is_empty());
    ctx.clock.advance(Duration::from_secs(31));
    let expired = ctx.meta.expire_capture_job_stages(&budgets).await.unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(
        expired[0].id, placed.id,
        "only placed over-budget rows expire"
    );
}

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

/// The dead-host straggler listing returns only HostLost sessions.
async fn list_host_lost_sessions(ctx: &Ctx) {
    let meta = &ctx.meta;
    let stranded = meta
        .create_session(spec("conf:host-lost-straggler"))
        .await
        .unwrap();
    let other = meta
        .create_session(spec("conf:not-host-lost"))
        .await
        .unwrap();

    meta.transition_session(stranded, SessionState::Created)
        .await
        .unwrap();
    meta.transition_session(stranded, SessionState::HostLost)
        .await
        .unwrap();

    let listed = meta.list_host_lost_sessions().await.unwrap();
    assert!(listed.iter().any(|session| session.id == stranded));
    assert!(!listed.iter().any(|session| session.id == other));
}

/// `terminate_session` picks the terminal target for the current state,
/// drives the (row-locked) transition, and is idempotent once terminal;
/// `notify_session_delta` is a best-effort ephemeral fan-out. Both stores
/// must agree (ADR 0098 D4) — Sim overrides the trait defaults rather than
/// silently inheriting them.
async fn terminate_and_delta(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta.create_session(spec("conf:terminate")).await.unwrap();
    meta.transition_session(id, SessionState::Created)
        .await
        .unwrap();
    meta.transition_session(id, SessionState::Active)
        .await
        .unwrap();

    // Best-effort delta fan-out never errors on a live store.
    meta.notify_session_delta(id, &serde_json::json!({"chunk": "hi"}))
        .await
        .unwrap();

    // Active is non-terminal: terminate routes to Completed and reports
    // the honest (prev, target) pair.
    let outcome = meta.terminate_session(id).await.unwrap();
    assert_eq!(
        outcome,
        Some((SessionState::Active, SessionState::Completed))
    );
    assert_eq!(
        meta.get_session(id).await.unwrap().status,
        SessionState::Completed
    );

    // Idempotent once terminal.
    assert_eq!(meta.terminate_session(id).await.unwrap(), None);

    // Missing row surfaces NotFound (not a silent None).
    let err = meta.terminate_session(SessionId::new()).await.unwrap_err();
    assert!(matches!(err, MetaError::NotFound));
}

/// `fc_snapshot_version_for_host` reads `hosts.capabilities.fc_snapshot_version`
/// off the active-host scan: present when the host reported one, `None` for a
/// host with no version and for an unknown host. Both stores must agree
/// (ADR 0098 D4).
async fn host_fc_snapshot_version(ctx: &Ctx) {
    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    let with_ver = HostId::new();
    let without_ver = HostId::new();

    let mut rec = host_record(with_ver, "conf-fcver", now);
    rec.capabilities.fc_snapshot_version = Some("v10.0.0".to_string());
    meta.upsert_host(rec).await.unwrap();
    meta.upsert_host(host_record(without_ver, "conf-nofcver", now))
        .await
        .unwrap();

    assert_eq!(
        meta.fc_snapshot_version_for_host(with_ver).await.unwrap(),
        Some("v10.0.0".to_string())
    );
    assert_eq!(
        meta.fc_snapshot_version_for_host(without_ver)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        meta.fc_snapshot_version_for_host(HostId::new())
            .await
            .unwrap(),
        None
    );
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

/// #800: `enqueue_evacuating_session_resume` — the RESERVED evac-placement
/// overflow CAS. Fenced `evacuating → queued` (resume-origin): matches only
/// on `status='evacuating'` AND the op's epoch; a wrong epoch, a wrong
/// status, and a second call are all clean no-ops. Both stores must agree.
async fn enqueue_evacuating_resume(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta.create_session(spec("conf:evacq")).await.unwrap();
    // Claim an op to establish the fencing epoch (as the evac resumer does).
    let EnqueueOutcome::Claimed(op) = meta
        .op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-a")
        .await
        .unwrap()
    else {
        panic!("claimed")
    };
    let epoch = op.epoch.unwrap();

    // Walk Pending → Created → Active → Evacuating (the resumer's input).
    for target in [
        SessionState::Created,
        SessionState::Active,
        SessionState::Evacuating,
    ] {
        meta.transition_session(sid, target).await.unwrap();
    }

    // Wrong epoch: no-op (a reclaimed-away zombie can't fork the machine).
    assert!(
        !meta
            .enqueue_evacuating_session_resume(sid, epoch + 1)
            .await
            .unwrap(),
        "epoch mismatch must not flip the row",
    );
    let still = meta.get_session(sid).await.unwrap();
    assert_eq!(still.status, SessionState::Evacuating);

    // Correct epoch + status: flips to queued (resume-origin), FIFO-visible.
    assert!(meta
        .enqueue_evacuating_session_resume(sid, epoch)
        .await
        .unwrap());
    let q = meta.list_queued_sessions_fifo().await.unwrap();
    let row = q
        .iter()
        .find(|r| r.session.id == sid)
        .expect("queued after evac overflow");
    assert!(matches!(
        row.origin,
        engram_core::types::session::QueueOrigin::Resume
    ));

    // Second call is a clean no-op — the row already left `evacuating`.
    assert!(
        !meta
            .enqueue_evacuating_session_resume(sid, epoch)
            .await
            .unwrap(),
        "a session no longer Evacuating must not re-queue",
    );

    // Wrong status guard: an Idle session is never eligible for this CAS.
    let other = meta.create_session(spec("conf:evacq2")).await.unwrap();
    let EnqueueOutcome::Claimed(op2) = meta
        .op_enqueue_and_claim(other, OpKind::Resume, serde_json::json!({}), None, "pod-a")
        .await
        .unwrap()
    else {
        panic!("claimed")
    };
    let epoch2 = op2.epoch.unwrap();
    for target in [
        SessionState::Created,
        SessionState::Active,
        SessionState::Idle,
    ] {
        meta.transition_session(other, target).await.unwrap();
    }
    assert!(
        !meta
            .enqueue_evacuating_session_resume(other, epoch2)
            .await
            .unwrap(),
        "an Idle session must not be evac-queued",
    );
    assert_eq!(
        meta.get_session(other).await.unwrap().status,
        SessionState::Idle,
    );
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

/// Issue #777 honest-Dead: `latest_snapshot_for_session` returns the
/// newest snapshot row AND its `recoverable` flag faithfully — the
/// foundation the unified HostLost stage-2 predicate
/// (`dead_host::recovery_target`) keys on. Pin it across BOTH stores so
/// the "un-recoverable-only ⇒ Dead, recoverable ⇒ Idle" decision rests
/// on identical store semantics: a session whose ONLY (or latest)
/// snapshot is `recoverable=false` must NOT masquerade as recoverable.
async fn latest_snapshot_reports_recoverable_flag(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta
        .create_session(spec("conf:latest-snap-recoverable"))
        .await
        .unwrap();

    // No snapshot yet → None (a genuinely never-checkpointed session;
    // the predicate routes this to Dead only when there's also no
    // manifest, and never lies it into Idle).
    assert!(meta
        .latest_snapshot_for_session(sid)
        .await
        .unwrap()
        .is_none());

    // The only snapshot is un-recoverable (a torn/HEAD-failed capture):
    // the row exists, but `recoverable` is false. The honest predicate
    // must see false here, not "a snapshot exists".
    let t0 = ctx.clock.now_utc();
    let bad = SnapshotId::new();
    assert!(meta
        .record_snapshot(snapshot(bad, sid, t0, false))
        .await
        .unwrap());
    let latest = meta
        .latest_snapshot_for_session(sid)
        .await
        .unwrap()
        .expect("a snapshot row exists");
    assert_eq!(latest.id, bad);
    assert!(
        !latest.recoverable,
        "an un-recoverable snapshot must report recoverable=false — honest-Dead (#777) keys on it",
    );

    // A newer recoverable snapshot becomes the latest and reports true —
    // now the same session IS recoverable (routes to Idle).
    ctx.clock.advance(Duration::from_secs(10));
    let t1 = ctx.clock.now_utc();
    let good = SnapshotId::new();
    assert!(meta
        .record_snapshot(snapshot(good, sid, t1, true))
        .await
        .unwrap());
    let latest = meta
        .latest_snapshot_for_session(sid)
        .await
        .unwrap()
        .expect("a snapshot row exists");
    assert_eq!(
        latest.id, good,
        "the newest row (by created_at) is the latest"
    );
    assert!(
        latest.recoverable,
        "a recoverable snapshot must report recoverable=true — the Idle arm keys on it",
    );
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

/// Register-time rehydrate list (`list_resident_sandboxes_on_host_
/// with_disk_manifest`): every VM-resident state with a bound sandbox
/// is returned — a rung-parked `Evicting` session included (session
/// 731df805, 2026-07-17: filtering on `status = 'active'` orphaned the
/// parked survivor's NBD device across a host-agent pod roll) — with
/// the effective disk manifest resolved as newer-of(live, latest
/// recoverable snapshot): same manifest_id → max(version), different
/// id → snapshot wins, non-recoverable snapshots invisible.
async fn resident_sandboxes_rehydrate_list(ctx: &Ctx) {
    use engram_core::types::manifest::ManifestRef;

    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    let host = HostId::new();
    let other_host = HostId::new();
    meta.upsert_host(host_record(host, "conf-rehydrate-h1", now))
        .await
        .unwrap();
    meta.upsert_host(host_record(other_host, "conf-rehydrate-h2", now))
        .await
        .unwrap();

    let mref = |manifest_id: uuid::Uuid, version: u64| ManifestRef {
        manifest_id,
        version,
    };
    let snap_with = |sid, at, recoverable, manifest: Option<ManifestRef>| {
        let mut s = snapshot(SnapshotId::new(), sid, at, recoverable);
        s.disk_manifest = manifest;
        s
    };
    let bind = |sid, h| async move {
        meta.assign_session_host(sid, Some(h)).await.unwrap();
        let sb = engram_core::SandboxId::new();
        meta.assign_session_sandbox(sid, Some(sb)).await.unwrap();
        meta.transition_session(sid, SessionState::Created)
            .await
            .unwrap();
        sb
    };

    let m_a = uuid::Uuid::from_u128(0xA);
    let m_b = uuid::Uuid::from_u128(0xB);
    let m_c = uuid::Uuid::from_u128(0xC);
    let m_d = uuid::Uuid::from_u128(0xD);
    let m_e = uuid::Uuid::from_u128(0xE);
    let m_f = uuid::Uuid::from_u128(0xF);

    // Active + sandbox: live A@5 vs recoverable snap A@3 → same id,
    // live is newer → A@5.
    let s_active = meta
        .create_session(spec("conf:rehydrate-active"))
        .await
        .unwrap();
    let sb_active = bind(s_active, host).await;
    meta.transition_session(s_active, SessionState::Active)
        .await
        .unwrap();
    meta.update_live_disk_manifest(s_active, sb_active, mref(m_a, 5))
        .await
        .unwrap();
    assert!(meta
        .record_snapshot(snap_with(s_active, now, true, Some(mref(m_a, 3))))
        .await
        .unwrap());

    // THE regression case — rung-parked survivor: Evicting + sandbox
    // still bound. Live B@2; two recoverable snapshots, the NEWEST
    // (D@1) must win over both the older snap (C@9) and the live ref
    // (different manifest_id → snapshot wins).
    let s_parked = meta
        .create_session(spec("conf:rehydrate-parked"))
        .await
        .unwrap();
    let sb_parked = bind(s_parked, host).await;
    meta.transition_session(s_parked, SessionState::Active)
        .await
        .unwrap();
    meta.update_live_disk_manifest(s_parked, sb_parked, mref(m_b, 2))
        .await
        .unwrap();
    assert!(meta
        .record_snapshot(snap_with(s_parked, now, true, Some(mref(m_c, 9))))
        .await
        .unwrap());
    ctx.clock.advance(Duration::from_secs(10));
    let later = ctx.clock.now_utc();
    assert!(meta
        .record_snapshot(snap_with(s_parked, later, true, Some(mref(m_d, 1))))
        .await
        .unwrap());
    meta.transition_session(s_parked, SessionState::Evicting)
        .await
        .unwrap();

    // Created + sandbox (VM resident, agent not yet started): no live
    // manifest; one recoverable snap E@1; a NEWER but non-recoverable
    // F@7 must be invisible → E@1.
    let s_created = meta
        .create_session(spec("conf:rehydrate-created"))
        .await
        .unwrap();
    let sb_created = bind(s_created, host).await;
    assert!(meta
        .record_snapshot(snap_with(s_created, now, true, Some(mref(m_e, 1))))
        .await
        .unwrap());
    assert!(meta
        .record_snapshot(snap_with(s_created, later, false, Some(mref(m_f, 7))))
        .await
        .unwrap());

    // Idle (evicted; sandbox unbound) → excluded.
    let s_idle = meta
        .create_session(spec("conf:rehydrate-idle"))
        .await
        .unwrap();
    bind(s_idle, host).await;
    meta.transition_session(s_idle, SessionState::Active)
        .await
        .unwrap();
    meta.transition_session(s_idle, SessionState::Idle)
        .await
        .unwrap();
    meta.assign_session_sandbox(s_idle, None).await.unwrap();

    // Active on ANOTHER host → excluded from this host's list.
    let s_elsewhere = meta
        .create_session(spec("conf:rehydrate-elsewhere"))
        .await
        .unwrap();
    bind(s_elsewhere, other_host).await;
    meta.transition_session(s_elsewhere, SessionState::Active)
        .await
        .unwrap();

    let mut rows = meta
        .list_resident_sandboxes_on_host_with_disk_manifest(host)
        .await
        .unwrap();
    rows.sort_by_key(|(sid, _, _)| *sid);
    let mut expected = vec![
        (s_active, sb_active, Some(mref(m_a, 5))),
        (s_parked, sb_parked, Some(mref(m_d, 1))),
        (s_created, sb_created, Some(mref(m_e, 1))),
    ];
    expected.sort_by_key(|(sid, _, _)| *sid);
    assert_eq!(rows, expected);
}

/// ADR 0023 broker tokens (R2): first-writer-wins insert, get, delete.
async fn broker_token_flow(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta.create_session(spec("conf:broker")).await.unwrap();
    let token = engram_core::types::registry::SessionBrokerToken {
        session_id: id,
        wrapped_dek: vec![1, 2, 3],
        nonce: vec![4, 5],
        ciphertext: vec![6, 7, 8, 9],
        key_id: "conf:kek:v1".to_string(),
    };
    assert!(meta.insert_broker_token(token.clone()).await.unwrap());
    // ON CONFLICT DO NOTHING: the racing sibling loses cleanly.
    assert!(!meta.insert_broker_token(token.clone()).await.unwrap());
    let got = meta.get_broker_token(id).await.unwrap().expect("present");
    assert_eq!(got.wrapped_dek, token.wrapped_dek);
    assert_eq!(got.key_id, token.key_id);
    meta.delete_broker_token(id).await.unwrap();
    assert!(meta.get_broker_token(id).await.unwrap().is_none());
    // A never-inserted session reads None (not an error).
    assert!(meta
        .get_broker_token(SessionId::new())
        .await
        .unwrap()
        .is_none());
}

/// ADR 0045 teleport target pin (R2): set stamps host+`_set_at`, clear
/// nulls both, get round-trips.
async fn teleport_target_flow(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta.create_session(spec("conf:teleport")).await.unwrap();
    assert!(meta.get_teleport_target(id).await.unwrap().is_none());
    let host = HostId::new();
    meta.set_teleport_target(id, Some(host)).await.unwrap();
    let (got_host, set_at) = meta.get_teleport_target(id).await.unwrap().expect("pinned");
    assert_eq!(got_host, host);
    assert!(set_at.is_some(), "a set pin stamps its set_at");
    meta.set_teleport_target(id, None).await.unwrap();
    assert!(meta.get_teleport_target(id).await.unwrap().is_none());
}

/// ADR 0101 C: the parked lifecycle + the durability-floor settle.
/// `parked` is a real state (`list_parked_sessions` finds it, the
/// eviction sweep does not), and `settle_evicted_session_idle` is a
/// single guarded settle: it flips `evicting → idle` + detaches ONLY
/// when the exact recoverable snapshot row exists and the exact sandbox
/// is still bound — every other combination is a clean `false` no-op.
async fn parked_lifecycle_and_eviction_settle(ctx: &Ctx) {
    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    let host = HostId::new();
    meta.upsert_host(host_record(host, "conf-parked-h1", now))
        .await
        .unwrap();

    let sid = meta.create_session(spec("conf:parked")).await.unwrap();
    meta.assign_session_host(sid, Some(host)).await.unwrap();
    let sb = engram_core::SandboxId::new();
    meta.assign_session_sandbox(sid, Some(sb)).await.unwrap();
    meta.transition_session(sid, SessionState::Created)
        .await
        .unwrap();
    meta.transition_session(sid, SessionState::Active)
        .await
        .unwrap();
    meta.transition_session(sid, SessionState::Evicting)
        .await
        .unwrap();
    meta.transition_session(sid, SessionState::Parked)
        .await
        .unwrap();

    // Parked is its own sweep — visible to the reaper's list, invisible
    // to the eviction scanner's.
    let parked = meta.list_parked_sessions().await.unwrap();
    assert_eq!(parked.len(), 1, "parked session is listed");
    assert_eq!(parked[0].id, sid);
    assert!(
        meta.list_evicting_sessions().await.unwrap().is_empty(),
        "a parked session is NOT an evicting row (the livelock class)"
    );

    // Descend: Parked → Evicting (the explicit nomination edge).
    meta.transition_session(sid, SessionState::Evicting)
        .await
        .unwrap();
    assert!(meta.list_parked_sessions().await.unwrap().is_empty());

    let snap_id = SnapshotId::new();
    // 1. No row yet → no settle.
    assert!(
        !meta
            .settle_evicted_session_idle(sid, sb, snap_id)
            .await
            .unwrap(),
        "no settle before the snapshot row exists"
    );
    // 2. A NON-recoverable row → no settle.
    assert!(meta
        .record_snapshot(snapshot(snap_id, sid, now, false))
        .await
        .unwrap());
    assert!(
        !meta
            .settle_evicted_session_idle(sid, sb, snap_id)
            .await
            .unwrap(),
        "a non-recoverable row must not settle Idle"
    );
    // 3. A recoverable row for a DIFFERENT session → no settle.
    let other = meta
        .create_session(spec("conf:parked-other"))
        .await
        .unwrap();
    let other_snap = SnapshotId::new();
    assert!(meta
        .record_snapshot(snapshot(other_snap, other, now, true))
        .await
        .unwrap());
    assert!(
        !meta
            .settle_evicted_session_idle(sid, sb, other_snap)
            .await
            .unwrap(),
        "another session's row must not settle this one"
    );
    // 4. The right row, but the WRONG sandbox (a rebound successor) → no
    //    settle.
    ctx.clock.advance(Duration::from_secs(1));
    let later = ctx.clock.now_utc();
    let good_snap = SnapshotId::new();
    assert!(meta
        .record_snapshot(snapshot(good_snap, sid, later, true))
        .await
        .unwrap());
    assert!(
        !meta
            .settle_evicted_session_idle(sid, engram_core::SandboxId::new(), good_snap)
            .await
            .unwrap(),
        "a stale advert against a rebound sandbox must not settle"
    );
    // 5. The exact triple → settle: idle + detached (host kept for
    //    resume affinity).
    assert!(meta
        .settle_evicted_session_idle(sid, sb, good_snap)
        .await
        .unwrap());
    let s = meta.get_session(sid).await.unwrap();
    assert_eq!(s.status, SessionState::Idle);
    assert_eq!(s.sandbox_id, None, "the settle detaches the sandbox");
    assert_eq!(s.host_id, Some(host), "host affinity preserved");
    // 6. Idempotent: a re-advert's second settle is a clean no-op.
    assert!(
        !meta
            .settle_evicted_session_idle(sid, sb, good_snap)
            .await
            .unwrap(),
        "an already-settled session no-ops"
    );

    // Parked → HostLost is the host-death edge (never Idle: the parked
    // RAM died with the host).
    let sid2 = meta.create_session(spec("conf:parked-lost")).await.unwrap();
    meta.assign_session_host(sid2, Some(host)).await.unwrap();
    meta.assign_session_sandbox(sid2, Some(engram_core::SandboxId::new()))
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::Created)
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::Active)
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::Evicting)
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::Parked)
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::HostLost)
        .await
        .unwrap();
    assert_eq!(
        meta.get_session(sid2).await.unwrap().status,
        SessionState::HostLost
    );
}

conformance!(t_broker_token_flow, super::broker_token_flow);
conformance!(
    t_parked_lifecycle_and_eviction_settle,
    super::parked_lifecycle_and_eviction_settle
);
conformance!(t_teleport_target_flow, super::teleport_target_flow);
conformance!(t_session_lifecycle, super::session_lifecycle);
conformance!(t_list_host_lost_sessions, super::list_host_lost_sessions);
conformance!(
    t_latest_snapshot_reports_recoverable_flag,
    super::latest_snapshot_reports_recoverable_flag
);
conformance!(t_terminate_and_delta, super::terminate_and_delta);
conformance!(t_host_fc_snapshot_version, super::host_fc_snapshot_version);
conformance!(
    t_enable_job_claim_boundary,
    super::enable_job_claim_boundary
);
conformance!(
    t_capture_job_scan_boundary,
    super::capture_job_scan_boundary
);
conformance!(t_queue_fifo, super::queue_fifo);
conformance!(t_dead_host_lease, super::dead_host_lease);
conformance!(t_ops_pipeline, super::ops_pipeline);
conformance!(t_ops_idempotency, super::ops_idempotency);
conformance!(t_fenced_transition, super::fenced_transition);
conformance!(
    t_enqueue_evacuating_resume,
    super::enqueue_evacuating_resume
);
conformance!(t_outbox_flow, super::outbox_flow);
conformance!(t_snapshot_durable_head, super::snapshot_durable_head);
conformance!(t_gc_candidates, super::gc_candidates);
conformance!(t_host_lifecycle, super::host_lifecycle);
conformance!(
    t_resident_sandboxes_rehydrate_list,
    super::resident_sandboxes_rehydrate_list
);

/// Issue #722 (R3): the reservation predicate is UNCONDITIONAL — a
/// `pending` pinned to a host holds its budget (visible through
/// placement_no_fit_details' free_mib) for as long as it is `pending`,
/// whether fresh, aged, live-op, or op-less. There is no crash-orphan
/// wall-age exclusion: an aged op-less orphan STILL reserves until the
/// ADR 0079 backstop reclaims it by a real `pending → failed` transition.
/// (Both stores must agree — the D4 conformance contract.)
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

    // Both reserve — 8192 - 2*2048 = 4096 free.
    let details = meta.placement_no_fit_details(&[host], 1, 1).await.unwrap();
    assert_eq!(details[0].free_mib, 4096);

    // R3 (#722): cross the old 10-minute horizon. BOTH pendings STILL
    // reserve — a `pending` holds its slot UNCONDITIONALLY (no wall-age /
    // op-liveness exclusion) until it LEAVES the reserving state. The old
    // predicate wrote off the op-less orphan here (6144 free), which let
    // placement re-sell a slot the ADR 0079 backstop could still revive
    // onto → Σ reserved > allocatable. The backstop reclaims a true orphan
    // by a real `pending → failed` transition (the sole reclaimer), never
    // a placement-side write-off.
    ctx.clock.advance(Duration::from_secs(11 * 60));
    let details = meta.placement_no_fit_details(&[host], 1, 1).await.unwrap();
    assert_eq!(
        details[0].free_mib, 4096,
        "R3 #722: a pending reserves unconditionally — neither the live-op \
         pending nor the aged op-less orphan may be written off while pending"
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
    // `enabled_images.base_snapshot_id` is NOT NULL + FK to `snapshots`
    // in PG (migration 0038): stage the session-less template base
    // snapshot first (migration 0028), exactly as the enable pipeline
    // does before it upserts the row.
    let base_id = SnapshotId::new();
    let mut base_snap = snapshot(base_id, SessionId::new(), at, true);
    base_snap.session_id = None;
    meta.record_snapshot(base_snap).await.unwrap();
    let mut img = enabled_image(uri, "before", at);
    img.base_snapshot_id = Some(base_id);
    // Denormalized manifest pair: NOT NULL since migration 0043.
    img.base_snapshot_disk_manifest = Some(engram_core::types::manifest::ManifestRef::new());
    img.base_snapshot_memory_manifest = Some(engram_core::types::manifest::ManifestRef::new());
    meta.upsert_enabled_image(img).await.unwrap();

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

/// ADR 0028 rung-1 rewind: `rewind_session_to_cursor` tombstones the
/// guest-derived tail past the checkpoint cursor, EXCLUDES the
/// coordinator-fact kinds (incl. `resume_started`, added with the
/// resume-progress event, and `durability_rollback`, ADR 0090), detects
/// surviving outside-world side-effects,
/// and bumps the recovery epoch only when something actually rolled back.
/// Pins the D4 conformance obligation for the exclusion-list SQL change.
async fn rewind_excludes_coordinator_facts(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta.create_session(spec("conf:rewind")).await.unwrap();

    // Anchor event AT the checkpoint cursor — never rolled back (idx <= cursor).
    let cursor = meta
        .append_session_event(id, "status_changed", serde_json::json!({"to": "idle"}))
        .await
        .unwrap();

    // The post-checkpoint tail: a mix of excluded coordinator facts and
    // rewindable guest-derived events, plus two surviving side-effects.
    meta.append_session_event(
        id,
        "prompt_received",
        serde_json::json!({"prompt_id": "p1"}),
    )
    .await
    .unwrap();
    meta.append_session_event(id, "resume_started", serde_json::json!({}))
        .await
        .unwrap();
    meta.append_session_event(id, "agent_message", serde_json::json!({"text": "hi"}))
        .await
        .unwrap();
    meta.append_session_event(
        id,
        "integration_asset",
        serde_json::json!({
            "provider": "forge",
            "asset_kind": "pull_request",
            "surface": "asset",
            "data": {"title": "PR #5"}
        }),
    )
    .await
    .unwrap();
    meta.append_session_event(id, "tool_call_started", serde_json::json!({"tool": "bash"}))
        .await
        .unwrap();
    meta.append_session_event(
        id,
        "file_shared",
        serde_json::json!({"caption": "my notes", "artifact_id": "art1"}),
    )
    .await
    .unwrap();
    meta.append_session_event(id, "harness_idle", serde_json::json!({}))
        .await
        .unwrap();
    // ADR 0090: the durability-rollback marker is a coordinator fact that
    // survives the very rewind it warns about — it must NOT tombstone.
    meta.append_session_event(
        id,
        "durability_rollback",
        serde_json::json!({
            "sandbox_id": "sb-1",
            "rewind_disk_manifest": {"manifest_id": "00000000-0000-0000-0000-000000000000", "version": 3},
            "reason": "quarantined-survivor evict budget exhausted; VM destroyed"
        }),
    )
    .await
    .unwrap();

    // Rewind everything after the anchor.
    let summary = meta.rewind_session_to_cursor(id, cursor).await.unwrap();

    // Only the guest-derived events roll back (agent_message,
    // integration_asset, tool_call_started, file_shared = 4); the
    // coordinator facts (prompt_received, resume_started, harness_idle,
    // durability_rollback) + the anchor status_changed survive.
    assert_eq!(summary.rolled_back, 4, "rolled_back count");
    assert_eq!(summary.through_idx, cursor, "through_idx is the cursor");
    assert_eq!(summary.recovery_epoch, 1, "epoch bumped once");
    assert_eq!(
        summary.surviving_side_effects,
        vec![
            "A forge pull_request was produced and still exists: PR #5".to_string(),
            "A file was shared and still exists: my notes".to_string(),
        ],
        "surviving side-effects, in idx order",
    );

    // Per-event tombstone state must agree across stores: excluded kinds
    // stay live (rewound_at None), rewindable kinds are tombstoned.
    let events = meta
        .list_session_events_since(id, cursor, 1000)
        .await
        .unwrap();
    for e in &events {
        let excluded = matches!(
            e.kind.as_str(),
            "prompt_received"
                | "resume_started"
                | "harness_idle"
                | "status_changed"
                | "durability_rollback"
        );
        assert_eq!(
            e.rewound_at.is_none(),
            excluded,
            "kind {} rewound_at (excluded={excluded})",
            e.kind
        );
    }

    // A newly appended event carries the bumped epoch.
    meta.append_session_event(id, "agent_message", serde_json::json!({"text": "post"}))
        .await
        .unwrap();
    let tail = meta
        .list_session_events_since(id, cursor, 1000)
        .await
        .unwrap();
    let post = tail.last().unwrap();
    assert_eq!(post.recovery_epoch, 1, "post-rewind event carries epoch 1");

    // A second rewind with the head already at/after the cursor rolls back
    // nothing and does NOT bump the epoch again (zero-rows early return).
    let head = post.idx;
    let again = meta.rewind_session_to_cursor(id, head).await.unwrap();
    assert_eq!(again.rolled_back, 0, "nothing left to roll back");
    assert_eq!(again.recovery_epoch, 0, "default summary on a no-op rewind");
    assert_eq!(again.surviving_side_effects, Vec::<String>::new());
}

conformance!(
    t_rewind_excludes_coordinator_facts,
    super::rewind_excludes_coordinator_facts
);
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
