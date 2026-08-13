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

use engram_core::types::event::EventCursor;
use engram_core::types::BindingDisposition;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::{Clock, MetadataStore};
use engram_core::types::capture_job::{
    CaptureJobReport, CaptureJobStage, CaptureTerminalReport, NewCaptureJob,
};
use engram_core::types::host::{
    HostCapacity, HostHeartbeat, HostLeaseState, HostRecord, HostStatus,
};
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

/// ADR 0103: durable re-attaches deduplicate lifecycle events by exec_id
/// (offsets legitimately remain `(0, 0)`), and `wall_ms` is measured from
/// the logged start. Every predicate clause is pinned: exec_id match,
/// session scoping, kind filter (other event kinds also carry `exec_id`
/// in their payloads), and MIN over duplicates.
async fn session_exec_event_at(ctx: &Ctx) {
    use engram_core::traits::ExecLifecycleEventKind::{Completed, Started};

    let sid = ctx
        .meta
        .create_session(spec("test.invalid/exec-started:latest"))
        .await
        .unwrap();
    let other_sid = ctx
        .meta
        .create_session(spec("test.invalid/exec-started-other:latest"))
        .await
        .unwrap();
    let exec_event = |kind: &str| {
        serde_json::json!({
            "type": kind,
            "exec_id": "exec:present",
            "command": ["true"],
            "at": ctx.clock.now_utc(),
        })
    };

    assert_eq!(
        ctx.meta
            .session_exec_event_at(sid, "exec:present", Started)
            .await
            .unwrap(),
        None,
        "an absent exec_id must not match"
    );

    // Same exec_id in a DIFFERENT session: the session_id clause is the
    // isolation boundary between sessions' exec dedup.
    ctx.meta
        .append_session_event(other_sid, "exec_started", exec_event("exec_started"))
        .await
        .unwrap();
    // Same exec_id under OTHER kinds in the same session: output and
    // completion events also carry `exec_id`, so only the kind filter
    // keeps them from satisfying a Started query.
    ctx.meta
        .append_session_event(
            sid,
            "stdout",
            serde_json::json!({ "type": "stdout", "exec_id": "exec:present", "chunk": "hi" }),
        )
        .await
        .unwrap();
    ctx.meta
        .append_session_event(sid, "exec_completed", exec_event("exec_completed"))
        .await
        .unwrap();
    assert_eq!(
        ctx.meta
            .session_exec_event_at(sid, "exec:present", Started)
            .await
            .unwrap(),
        None,
        "another session's exec_started and this session's non-started kinds must not match"
    );

    let first_started_at = ctx.clock.now_utc();
    ctx.meta
        .append_session_event(sid, "exec_started", exec_event("exec_started"))
        .await
        .unwrap();
    ctx.clock.advance(Duration::from_millis(1500));
    // A residual concurrent-attach duplicate must not move the timestamp:
    // the earliest occurrence wins (SQL MIN).
    ctx.meta
        .append_session_event(sid, "exec_started", exec_event("exec_started"))
        .await
        .unwrap();

    assert_eq!(
        ctx.meta
            .session_exec_event_at(sid, "exec:present", Started)
            .await
            .unwrap(),
        Some(first_started_at),
        "the persisted exec_id must match at its FIRST logged timestamp"
    );
    assert_eq!(
        ctx.meta
            .session_exec_event_at(sid, "exec:different", Started)
            .await
            .unwrap(),
        None,
        "a different exec_id must not match"
    );
    assert!(
        ctx.meta
            .session_exec_event_at(sid, "exec:present", Completed)
            .await
            .unwrap()
            .is_some(),
        "the Completed kind resolves independently of Started"
    );

    // The lookup returns the event's OWN `at` stamp, not the row's
    // created_at. A silent command's exec_started row lands only at its
    // first delivered frame (the Exit itself) while its `at` records the
    // attach time — measuring from created_at would collapse wall_ms to ~0.
    let skewed_at = ctx.clock.now_utc() - chrono::Duration::seconds(30);
    ctx.meta
        .append_session_event(
            sid,
            "exec_started",
            serde_json::json!({
                "type": "exec_started",
                "exec_id": "exec:skewed",
                "command": ["true"],
                "at": skewed_at,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        ctx.meta
            .session_exec_event_at(sid, "exec:skewed", Started)
            .await
            .unwrap(),
        Some(skewed_at),
        "the lookup must return the event's recorded `at`, not the row's created_at"
    );

    // An ADR-0028 recovery rewind tombstones exec lifecycle rows. The guest
    // journal rewinds with the disk, so a replayed step legitimately re-runs
    // the same ticket — the dedup must see the LIVE timeline only, or the
    // re-run leaves no lifecycle record at all.
    ctx.meta.rewind_session_to_cursor(sid, 0).await.unwrap();
    assert_eq!(
        ctx.meta
            .session_exec_event_at(sid, "exec:present", Started)
            .await
            .unwrap(),
        None,
        "tombstoned (rewound) rows must not satisfy the dedup lookup"
    );
    assert_eq!(
        ctx.meta
            .session_exec_event_at(sid, "exec:present", Completed)
            .await
            .unwrap(),
        None,
        "tombstoned (rewound) completions must not satisfy the dedup lookup"
    );
}

/// ADR 0103: output recording is observation-independent — re-attaches skip
/// persisting at or below the recorded high-water mark. Pins every predicate
/// clause: byte-range stamps (unstamped legacy rows invisible), per-stream
/// separation, per-exec and per-session isolation, and MAX over rows.
async fn session_exec_output_high_water(ctx: &Ctx) {
    use engram_core::traits::metadata::ExecOutputStream::{Stderr, Stdout};

    let sid = ctx
        .meta
        .create_session(spec("test.invalid/exec-highwater:latest"))
        .await
        .unwrap();
    let other_sid = ctx
        .meta
        .create_session(spec("test.invalid/exec-highwater-other:latest"))
        .await
        .unwrap();
    let chunk = |exec_id: &str, start: u64, end: u64| {
        serde_json::json!({
            "type": "stdout",
            "exec_id": exec_id,
            "chunk": "x",
            "bytes_start": start,
            "bytes_end": end,
        })
    };

    assert_eq!(
        ctx.meta
            .session_exec_output_high_water(sid, "exec:hw", Stdout)
            .await
            .unwrap(),
        0,
        "no rows → zero"
    );

    // Legacy row without stamps: invisible to the mark.
    ctx.meta
        .append_session_event(
            sid,
            "stdout",
            serde_json::json!({ "type": "stdout", "exec_id": "exec:hw", "chunk": "legacy" }),
        )
        .await
        .unwrap();
    // Foreign rows that must not count: another exec, another session, and
    // the other stream.
    ctx.meta
        .append_session_event(sid, "stdout", chunk("exec:other", 0, 99_999))
        .await
        .unwrap();
    ctx.meta
        .append_session_event(other_sid, "stdout", chunk("exec:hw", 0, 77_777))
        .await
        .unwrap();
    ctx.meta
        .append_session_event(
            sid,
            "stderr",
            serde_json::json!({
                "type": "stderr",
                "exec_id": "exec:hw",
                "chunk": "e",
                "bytes_start": 0,
                "bytes_end": 999,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        ctx.meta
            .session_exec_output_high_water(sid, "exec:hw", Stdout)
            .await
            .unwrap(),
        0,
        "legacy/foreign/other-stream rows must not move the stdout mark"
    );

    ctx.meta
        .append_session_event(sid, "stdout", chunk("exec:hw", 0, 4096))
        .await
        .unwrap();
    ctx.meta
        .append_session_event(sid, "stdout", chunk("exec:hw", 4096, 8192))
        .await
        .unwrap();
    assert_eq!(
        ctx.meta
            .session_exec_output_high_water(sid, "exec:hw", Stdout)
            .await
            .unwrap(),
        8192,
        "the mark is the MAX stamped end offset"
    );
    assert_eq!(
        ctx.meta
            .session_exec_output_high_water(sid, "exec:hw", Stderr)
            .await
            .unwrap(),
        999,
        "streams carry independent marks"
    );

    // After a recovery rewind the guest re-runs the ticket and its output
    // must be re-recorded — tombstoned rows must not hold the mark up.
    ctx.meta.rewind_session_to_cursor(sid, 0).await.unwrap();
    assert_eq!(
        ctx.meta
            .session_exec_output_high_water(sid, "exec:hw", Stdout)
            .await
            .unwrap(),
        0,
        "tombstoned (rewound) chunk rows must not satisfy the high-water mark"
    );
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
        sandbox_bundles: Vec::new(),
        cordoned: false,
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
        lease_expires_at: None,
        lease_state: Default::default(),
        lease_epoch: 0,
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
        sandbox_bundles: Vec::new(),
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
        lease_renew_until: None,
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
        .transition_session(id, SessionState::Active, BindingDisposition::Retain)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)), "got {err:?}");

    // Pending -> Failed is legal and returns the previous state.
    let prev = meta
        .transition_session(id, SessionState::Failed, BindingDisposition::Detach)
        .await
        .unwrap();
    assert_eq!(prev, SessionState::Pending);

    // Terminal: no exits.
    let err = meta
        .transition_session(id, SessionState::Queued, BindingDisposition::Detach)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)));

    // Missing row.
    let err = meta
        .transition_session(
            SessionId::new(),
            SessionState::Failed,
            BindingDisposition::Detach,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::NotFound));
}

/// #896 / ADR 0090 addendum: the BindingDisposition contract — checked
/// identically by both stores, under the same lock as state legality.
async fn binding_disposition_contract(ctx: &Ctx) {
    let meta = &ctx.meta;
    // A bound row: create -> Created (transition_session_created binds).
    let id = meta.create_session(spec("conf:disposition")).await.unwrap();
    let sb = engram_core::SandboxId::new();
    meta.transition_session_created(id, sb).await.unwrap();
    meta.transition_session(id, SessionState::Active, BindingDisposition::Retain)
        .await
        .unwrap();

    // (1) Retain into a forbidden target on a bound row => Conflict, and
    // NOTHING committed (state and binding unchanged).
    let err = meta
        .transition_session(id, SessionState::Failed, BindingDisposition::Retain)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)), "got {err:?}");
    let s = meta.get_session(id).await.unwrap();
    assert_eq!(s.status, SessionState::Active);
    assert_eq!(s.sandbox_id, Some(sb));

    // (4) RequireUnbound on a bound row => Conflict; on an unbound row it
    // holds (checked further below on the evacuating->idle residue path).
    let err = meta
        .transition_session(
            id,
            SessionState::Evacuating,
            BindingDisposition::RequireUnbound,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)), "got {err:?}");

    // (3) The residue lane: Evacuating retains; Retain -> Idle keeps the
    // binding (the ONE authorized bound-Idle arrival).
    meta.transition_session(id, SessionState::Evacuating, BindingDisposition::Retain)
        .await
        .unwrap();
    let prev = meta
        .transition_session(id, SessionState::Idle, BindingDisposition::Retain)
        .await
        .unwrap();
    assert_eq!(prev, SessionState::Evacuating);
    let s = meta.get_session(id).await.unwrap();
    assert_eq!(s.status, SessionState::Idle);
    assert_eq!(s.sandbox_id, Some(sb), "the residue retains the binding");

    // (2) Detach into a terminal clears the binding in the SAME write and
    // returns the previous state.
    let prev = meta
        .transition_session(id, SessionState::Dead, BindingDisposition::Detach)
        .await
        .unwrap();
    assert_eq!(prev, SessionState::Idle);
    let s = meta.get_session(id).await.unwrap();
    assert_eq!(s.status, SessionState::Dead);
    assert_eq!(s.sandbox_id, None, "Detach clears atomically with the flip");

    // (5) The fenced variants: same contract under a matching epoch; a
    // fenced-out write leaves state AND binding untouched.
    let id2 = meta
        .create_session(spec("conf:disposition-fenced"))
        .await
        .unwrap();
    let sb2 = engram_core::SandboxId::new();
    meta.transition_session_created(id2, sb2).await.unwrap();
    let epoch = {
        // Claim an op to establish a real epoch (the fenced predicate).
        let out = meta
            .op_enqueue_and_claim(id2, OpKind::Resume, serde_json::json!({}), None, "conf-pod")
            .await
            .unwrap();
        match out {
            EnqueueOutcome::Claimed(op) => op.epoch.unwrap(),
            other => panic!("expected a claim, got {other:?}"),
        }
    };
    let err = meta
        .fenced_transition_session(
            id2,
            epoch,
            SessionState::Completed,
            BindingDisposition::Retain,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)), "got {err:?}");
    let fenced = meta
        .fenced_transition_session(
            id2,
            epoch + 1,
            SessionState::Active,
            BindingDisposition::Detach,
        )
        .await
        .unwrap();
    assert!(fenced.is_none(), "stale epoch is a silent stop");
    let s = meta.get_session(id2).await.unwrap();
    assert_eq!(
        s.sandbox_id,
        Some(sb2),
        "a fenced-out write leaves the binding"
    );
    let prev = meta
        .fenced_transition_session(id2, epoch, SessionState::Active, BindingDisposition::Retain)
        .await
        .unwrap();
    assert_eq!(prev, Some(SessionState::Created));
    let prev = meta
        .fenced_transition_session_with_events(
            id2,
            epoch,
            SessionState::Completed,
            BindingDisposition::Detach,
            &[("status_changed".to_string(), serde_json::json!({}))],
        )
        .await
        .unwrap();
    assert!(prev.is_some());
    let s = meta.get_session(id2).await.unwrap();
    assert_eq!(s.status, SessionState::Completed);
    assert_eq!(s.sandbox_id, None);
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

    meta.transition_session(stranded, SessionState::Created, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.transition_session(stranded, SessionState::HostLost, BindingDisposition::Retain)
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
    meta.transition_session(id, SessionState::Created, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.transition_session(id, SessionState::Active, BindingDisposition::Retain)
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
        oauth_binding: None,
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
    meta.transition_session(c, SessionState::Queued, BindingDisposition::Detach)
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

/// ADR 0116 A-D1..D4: the host binding lease — register replaces + bumps
/// the epoch + ends a handoff; heartbeat renews with GREATEST and never
/// demotes a handoff; handoff keeps the max deadline; the expiry list
/// treats a NULL lease as expired (no lease ⇒ no shield), ignores
/// cordons, and excludes future handoffs; `renew_host_lease` (the
/// durable probe-rescue) GREATEST-extends, promotes `none`→`active`,
/// never demotes a handoff, and refuses dead/unknown rows.
async fn host_binding_lease(ctx: &Ctx) {
    let meta = &ctx.meta;
    let ttl = chrono::Duration::seconds(45);
    let host = HostId::new();

    let fetch = |meta: std::sync::Arc<dyn MetadataStore>, host: HostId| async move {
        meta.list_active_hosts()
            .await
            .unwrap()
            .into_iter()
            .find(|h| h.id == host)
            .expect("host row")
    };

    // Register: deadline replaced, Active, epoch 1.
    let t0 = ctx.clock.now_utc();
    let mut rec = host_record(host, "lease-host", t0);
    rec.lease_expires_at = Some(t0 + ttl);
    meta.upsert_host(rec.clone()).await.unwrap();
    let row = fetch(meta.clone(), host).await;
    assert_eq!(row.lease_expires_at, Some(t0 + ttl));
    assert_eq!(row.lease_state, HostLeaseState::Active);
    assert_eq!(row.lease_epoch, 1);

    // Heartbeat renew extends (GREATEST over a later target).
    ctx.clock.advance(Duration::from_secs(10));
    let t1 = ctx.clock.now_utc();
    let mut hb = heartbeat_fixture();
    hb.lease_renew_until = Some(t1 + ttl);
    meta.touch_host_heartbeat(host, hb.clone()).await.unwrap();
    let row = fetch(meta.clone(), host).await;
    assert_eq!(row.lease_expires_at, Some(t1 + ttl), "renew extends");
    assert_eq!(row.lease_state, HostLeaseState::Active);

    // Handoff: max deadline, state flips; a later racing heartbeat renew
    // must neither shrink the deadline nor demote the state.
    let handoff_until = t1 + chrono::Duration::seconds(600);
    assert!(meta.begin_host_handoff(host, handoff_until).await.unwrap());
    let mut hb2 = heartbeat_fixture();
    hb2.lease_renew_until = Some(t1 + ttl);
    meta.touch_host_heartbeat(host, hb2).await.unwrap();
    let row = fetch(meta.clone(), host).await;
    assert_eq!(
        row.lease_expires_at,
        Some(handoff_until),
        "a predecessor's racing heartbeat never shrinks a handoff deadline"
    );
    assert_eq!(
        row.lease_state,
        HostLeaseState::Handoff,
        "a heartbeat never demotes a declared handoff"
    );
    // Repeated handoff with an EARLIER deadline keeps the max.
    assert!(meta
        .begin_host_handoff(host, t1 + chrono::Duration::seconds(60))
        .await
        .unwrap());
    let row = fetch(meta.clone(), host).await;
    assert_eq!(row.lease_expires_at, Some(handoff_until));

    // Successor register: REPLACES the (longer) handoff deadline —
    // successor presence ends the handoff — Active, epoch 2.
    ctx.clock.advance(Duration::from_secs(20));
    let t2 = ctx.clock.now_utc();
    let mut rec2 = host_record(host, "lease-host", t2);
    rec2.lease_expires_at = Some(t2 + ttl);
    meta.upsert_host(rec2).await.unwrap();
    let row = fetch(meta.clone(), host).await;
    assert_eq!(row.lease_expires_at, Some(t2 + ttl), "register replaces");
    assert_eq!(row.lease_state, HostLeaseState::Active);
    assert_eq!(row.lease_epoch, 2);

    // Expiry list: a NULL-lease ready row is a candidate IMMEDIATELY
    // (no lease ⇒ no shield), cordoned or not; a leased host is
    // shielded until its deadline.
    let unleased = HostId::new();
    meta.upsert_host(host_record(unleased, "unleased-host", t2))
        .await
        .unwrap();
    meta.set_host_cordoned(unleased, true).await.unwrap();
    let expired: Vec<HostId> = meta
        .list_lease_expired_hosts()
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect();
    assert!(
        expired.contains(&unleased),
        "a NULL lease reads as expired; cordon grants no shield"
    );
    assert!(!expired.contains(&host), "live lease shields");

    // The durable probe-rescue: renew writes the unleased host's FIRST
    // lease (`none` → `active`) and takes it off the candidate list.
    assert!(meta.renew_host_lease(unleased, t2 + ttl).await.unwrap());
    let row = fetch(meta.clone(), unleased).await;
    assert_eq!(row.lease_expires_at, Some(t2 + ttl));
    assert_eq!(row.lease_state, HostLeaseState::Active);
    let expired = meta.list_lease_expired_hosts().await.unwrap();
    assert!(expired.is_empty(), "rescue shields: {expired:?}");

    ctx.clock.advance(Duration::from_secs(46));
    let expired: Vec<HostId> = meta
        .list_lease_expired_hosts()
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect();
    assert!(
        expired.contains(&host),
        "leased host expires at its deadline"
    );
    assert!(expired.contains(&unleased), "rescue expires at its TTL");

    // A future handoff shields a host from the expiry list, and a
    // racing rescue with an earlier deadline never shrinks it.
    let t3 = ctx.clock.now_utc();
    let handoff_shield = t3 + chrono::Duration::seconds(600);
    assert!(meta.begin_host_handoff(host, handoff_shield).await.unwrap());
    assert!(meta.renew_host_lease(host, t3 + ttl).await.unwrap());
    let row = fetch(meta.clone(), host).await;
    assert_eq!(
        row.lease_expires_at,
        Some(handoff_shield),
        "a rescue never shrinks a handoff deadline"
    );
    assert_eq!(
        row.lease_state,
        HostLeaseState::Handoff,
        "a rescue never demotes a declared handoff"
    );
    let expired: Vec<HostId> = meta
        .list_lease_expired_hosts()
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect();
    assert!(
        !expired.contains(&host),
        "handoff shields until its deadline"
    );

    // Handoff and rescue on an unknown host: false. On a dead host: false.
    assert!(!meta.begin_host_handoff(HostId::new(), t3).await.unwrap());
    assert!(!meta
        .renew_host_lease(HostId::new(), t3 + ttl)
        .await
        .unwrap());
    meta.set_host_status(unleased, HostStatus::Dead)
        .await
        .unwrap();
    assert!(!meta.begin_host_handoff(unleased, t3).await.unwrap());
    assert!(!meta.renew_host_lease(unleased, t3 + ttl).await.unwrap());
}

/// ADR 0116 A-D5: sandbox tombstones — the bulk orphan writes one per
/// cleared binding in the SAME transaction; `record_sandbox_tombstone`
/// is idempotent; `ack_sandbox_tombstones_by_absence` deletes exactly
/// the rows whose sandbox left the host's reported running set.
async fn sandbox_tombstones(ctx: &Ctx) {
    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    let host = HostId::new();
    meta.upsert_host(host_record(host, "tomb-host", now))
        .await
        .unwrap();

    // A bound session on the host; the bulk orphan must entomb its
    // sandbox in-tx.
    let sid = meta.create_session(spec("conf:tombstone")).await.unwrap();
    meta.assign_session_host(sid, Some(host)).await.unwrap();
    let sb = engram_core::SandboxId::new();
    meta.transition_session_created(sid, sb).await.unwrap();
    let affected = meta.mark_host_dead_if_lease_expired(host).await.unwrap();
    assert_eq!(affected.len(), 1);
    assert_eq!(
        meta.sandbox_tombstones_for_host(host).await.unwrap(),
        vec![sb],
        "the bulk orphan writes a tombstone per cleared binding"
    );

    // Idempotent re-record + a second, session-less tombstone.
    meta.record_sandbox_tombstone(host, sb, Some(sid))
        .await
        .unwrap();
    let sb2 = engram_core::SandboxId::new();
    meta.record_sandbox_tombstone(host, sb2, None)
        .await
        .unwrap();
    let mut listed = meta.sandbox_tombstones_for_host(host).await.unwrap();
    listed.sort();
    let mut expected = vec![sb, sb2];
    expected.sort();
    assert_eq!(listed, expected);

    // Ack-by-absence: a sandbox STILL in the running set keeps its row;
    // one absent from it is acked (deleted).
    let acked = meta
        .ack_sandbox_tombstones_by_absence(host, &[sb])
        .await
        .unwrap();
    assert_eq!(acked, vec![sb2], "only the absent sandbox is acked");
    assert_eq!(
        meta.sandbox_tombstones_for_host(host).await.unwrap(),
        vec![sb]
    );
    let acked = meta
        .ack_sandbox_tombstones_by_absence(host, &[])
        .await
        .unwrap();
    assert_eq!(acked, vec![sb]);
    assert!(meta
        .sandbox_tombstones_for_host(host)
        .await
        .unwrap()
        .is_empty());

    // Unknown host: nothing outstanding, nothing acked.
    assert!(meta
        .sandbox_tombstones_for_host(HostId::new())
        .await
        .unwrap()
        .is_empty());

    // ADR 0116 A5 rebind-supersede: rebinding a session onto a fresh
    // sandbox entombs the superseded one in the same write.
    let sid2 = meta.create_session(spec("conf:rebind")).await.unwrap();
    meta.assign_session_host(sid2, Some(host)).await.unwrap();
    let old_sb = engram_core::SandboxId::new();
    meta.transition_session_created(sid2, old_sb).await.unwrap();
    let new_sb = engram_core::SandboxId::new();
    meta.rebind_session_guarded(sid2, host, new_sb, Some(Some(old_sb)), &[])
        .await
        .unwrap();
    assert!(
        meta.sandbox_tombstones_for_host(host)
            .await
            .unwrap()
            .contains(&old_sb),
        "a rebind entombs the superseded binding"
    );
    meta.ack_sandbox_tombstones_by_absence(host, &[])
        .await
        .unwrap();

    // ADR 0116 A5 lost-destroy leftover: a running sandbox NO session
    // binds is sighted, spared inside the grace, and entombed once the
    // sighting is stable past it; a BOUND running sandbox never is.
    let ghost = engram_core::SandboxId::new();
    let entombed = meta
        .entomb_stably_unbound(host, &[ghost, new_sb], 30)
        .await
        .unwrap();
    assert!(entombed.is_empty(), "a fresh sighting is inside the grace");
    ctx.clock.advance(Duration::from_secs(31));
    let entombed = meta
        .entomb_stably_unbound(host, &[ghost, new_sb], 30)
        .await
        .unwrap();
    assert_eq!(
        entombed,
        vec![ghost],
        "stably unbound graduates; bound never"
    );
    assert!(meta
        .sandbox_tombstones_for_host(host)
        .await
        .unwrap()
        .contains(&ghost));
    // A sighting whose sandbox becomes bound (or leaves the set) is
    // pruned — it must NOT graduate later from a stale stamp.
    let late_bind = engram_core::SandboxId::new();
    let none = meta
        .entomb_stably_unbound(host, &[late_bind], 30)
        .await
        .unwrap();
    assert!(none.is_empty());
    let sid3 = meta.create_session(spec("conf:latebind")).await.unwrap();
    meta.assign_session_host(sid3, Some(host)).await.unwrap();
    meta.transition_session_created(sid3, late_bind)
        .await
        .unwrap();
    ctx.clock.advance(Duration::from_secs(31));
    let none = meta
        .entomb_stably_unbound(host, &[late_bind], 30)
        .await
        .unwrap();
    assert!(
        none.is_empty(),
        "a now-bound sandbox is pruned, never entombed"
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
/// ADR 0101 C (engrams review, #836 rounds 2+3): `op_latest_for_kind` —
/// the newest mint of a kind for the session, ANY state, ANY key. Two
/// properties are the point: TERMINAL rows stay visible (the dedup
/// index forgets them by design; the eviction scanner must see "a
/// capture already landed" without re-minting), and the read is
/// KEY-AGNOSTIC (the three post-capture paths mint under three
/// different keys — one of them under none at all).
async fn op_latest_for_kind_reads_terminal_mints(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta.create_session(spec("conf:oplatest")).await.unwrap();
    assert!(
        meta.op_latest_for_kind(sid, OpKind::Evict)
            .await
            .unwrap()
            .is_none(),
        "no mints → None"
    );
    // A KEYLESS mint (the admin/evict_local shape) — must be visible.
    let EnqueueOutcome::Claimed(op) = meta
        .op_enqueue_and_claim(sid, OpKind::Evict, serde_json::json!({}), None, "pod-a")
        .await
        .unwrap()
    else {
        panic!("claimed")
    };
    assert!(meta
        .op_finish(op.id, op.epoch.unwrap(), OpState::Done, None)
        .await
        .unwrap());
    let latest = meta
        .op_latest_for_kind(sid, OpKind::Evict)
        .await
        .unwrap()
        .expect("a terminal, keyless mint is visible — that is the method's point");
    assert_eq!(latest.id, op.id);
    assert_eq!(latest.state, OpState::Done);
    assert!(
        latest.finished_at.is_some(),
        "finished_at stamps on finish (the scanner's grace check reads it)"
    );
    // A newer mint under a KEY (the descent shape) becomes the newest.
    let EnqueueOutcome::Claimed(op2) = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("evict-descend:42"),
            "pod-a",
        )
        .await
        .unwrap()
    else {
        panic!("descent mint claimed")
    };
    let latest = meta
        .op_latest_for_kind(sid, OpKind::Evict)
        .await
        .unwrap()
        .expect("still visible");
    assert_eq!(latest.id, op2.id, "newest mint wins, key or no key");
    // Kind + session isolation.
    assert!(meta
        .op_latest_for_kind(sid, OpKind::Resume)
        .await
        .unwrap()
        .is_none());
    let other = meta.create_session(spec("conf:oplatest-b")).await.unwrap();
    assert!(meta
        .op_latest_for_kind(other, OpKind::Evict)
        .await
        .unwrap()
        .is_none());
}

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
        .fenced_transition_session(
            sid,
            epoch + 1,
            SessionState::Failed,
            BindingDisposition::Detach
        )
        .await
        .unwrap()
        .is_none());
    let err = meta
        .fenced_transition_session(sid, epoch, SessionState::Active, BindingDisposition::Retain)
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)));
    let prev = meta
        .fenced_transition_session(sid, epoch, SessionState::Failed, BindingDisposition::Detach)
        .await
        .unwrap();
    assert_eq!(prev, Some(SessionState::Pending));
}

/// Atomic fenced transition + lifecycle events (the eviction event-loss
/// fix): a stale epoch or an illegal transition commits NOTHING — no
/// state flip, no events, no index burn; success lands the flip and the
/// events in order with contiguous indices. Both stores must agree.
async fn fenced_transition_with_events(ctx: &Ctx) {
    let meta = &ctx.meta;
    let sid = meta.create_session(spec("conf:fence-ev")).await.unwrap();
    let EnqueueOutcome::Claimed(op) = meta
        .op_enqueue_and_claim(sid, OpKind::Evict, serde_json::json!({}), None, "pod-a")
        .await
        .unwrap()
    else {
        panic!("claimed")
    };
    let epoch = op.epoch.unwrap();

    // Anchor the index sequence with a plain append, and bind a sandbox
    // so the success arm can prove the atomic detach.
    let baseline = meta
        .append_session_event(sid, "status_changed", serde_json::json!({"probe": true}))
        .await
        .unwrap();
    let sb = engram_core::SandboxId::new();
    // 0108: bind via the production fused path, never on a Pending row.
    meta.transition_session_created(sid, sb).await.unwrap();
    let events = vec![
        ("evicted".to_string(), serde_json::json!({"at": "t0"})),
        (
            "status_changed".to_string(),
            serde_json::json!({"to": "failed"}),
        ),
    ];

    // Stale epoch: silent None, nothing lands.
    assert!(meta
        .fenced_transition_session_with_events(
            sid,
            epoch + 1,
            SessionState::Failed,
            BindingDisposition::Detach,
            &events
        )
        .await
        .unwrap()
        .is_none());
    // Illegal transition (Created → Queued; Created → Idle became the
    // legal ADR 0116 B-D3 re-plan edge): Conflict, nothing lands.
    let err = meta
        .fenced_transition_session_with_events(
            sid,
            epoch,
            SessionState::Queued,
            BindingDisposition::Detach,
            &events,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, MetaError::Conflict(_)));
    assert_eq!(
        meta.get_session(sid).await.unwrap().status,
        SessionState::Created,
        "rejected calls must not flip state",
    );
    let leaked = meta
        .list_session_events_since(sid, baseline, 100)
        .await
        .unwrap();
    assert!(
        leaked.is_empty(),
        "rejected calls must append nothing: {leaked:?}",
    );
    assert_eq!(
        meta.get_session(sid).await.unwrap().sandbox_id,
        Some(sb),
        "rejected calls must not detach",
    );

    // Matching epoch: the flip and both events land together, indices
    // contiguous with the baseline append.
    let (prev, idxs) = meta
        .fenced_transition_session_with_events(
            sid,
            epoch,
            SessionState::Failed,
            BindingDisposition::Detach,
            &events,
        )
        .await
        .unwrap()
        .expect("matching epoch must land");
    assert_eq!(prev, SessionState::Created);
    assert_eq!(idxs, vec![baseline + 1, baseline + 2]);
    let settled = meta.get_session(sid).await.unwrap();
    assert_eq!(settled.status, SessionState::Failed);
    assert_eq!(
        settled.sandbox_id, None,
        "detach_sandbox rides the same transaction as the flip",
    );
    let landed = meta
        .list_session_events_since(sid, baseline, 100)
        .await
        .unwrap();
    assert_eq!(landed.len(), 2, "exactly the two events: {landed:?}");
    assert_eq!(
        (landed[0].idx, landed[0].kind.as_str()),
        (baseline + 1, "evicted")
    );
    assert_eq!(
        (landed[1].idx, landed[1].kind.as_str()),
        (baseline + 2, "status_changed"),
    );
    assert_eq!(landed[1].payload, serde_json::json!({"to": "failed"}));
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
        meta.transition_session(sid, target, BindingDisposition::Retain)
            .await
            .unwrap();
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
        meta.transition_session(other, target, BindingDisposition::Retain)
            .await
            .unwrap();
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
/// once-only; delivered rows can't be deleted as undelivered;
/// make_due (ADR 0108 A8) recalls exactly the waiting un-acked rows,
/// idempotently and without an attempts bump.
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

    // ADR 0108: `outbox_make_due` — the inverse of defer. Stage a
    // deferred row (p-2), a delivered-but-unacked row (p-3), and an
    // already-due row (p-4); p-1 above is acked. make_due must pull
    // exactly the two waiting rows, bump no `attempts`, and be a
    // no-op on a repeat call.
    let now = ctx.clock.now_utc();
    for (i, pid) in ["p-2", "p-3", "p-4"].iter().enumerate() {
        meta.outbox_enqueue(&engram_core::types::outbox::OutboxRow {
            prompt_id: (*pid).into(),
            session_id: sid,
            kind: engram_core::types::outbox::OutboxKind::Prompt,
            payload: serde_json::json!({"text": "hi"}),
            // Staggered so per-row assertions below can walk
            // next_due order deterministically on both stores.
            created_at: now + chrono::Duration::milliseconds(i as i64),
            attempts: 0,
            not_before: now,
            delivered_at: None,
            acked_at: None,
        })
        .await
        .unwrap();
    }
    meta.outbox_defer("p-2", Duration::from_secs(60))
        .await
        .unwrap(); // attempts -> 1
    meta.outbox_mark_delivered("p-3", Duration::from_secs(30))
        .await
        .unwrap(); // attempts -> 1
    assert_eq!(
        meta.outbox_make_due(sid).await.unwrap(),
        2,
        "make_due moves the deferred + the delivered-unacked row; not the due row, not the acked row"
    );
    assert_eq!(
        meta.outbox_make_due(sid).await.unwrap(),
        0,
        "idempotent: nothing left with a future not_before"
    );
    // Walk next_due (oldest created_at first): every row is due NOW
    // and make_due changed no `attempts`.
    for (pid, attempts) in [("p-2", 1), ("p-3", 1), ("p-4", 0)] {
        let due = meta.outbox_next_due(sid).await.unwrap().expect("due row");
        assert_eq!(due.prompt_id, pid);
        assert_eq!(due.attempts, attempts, "make_due must not bump attempts");
        assert!(meta.outbox_ack(pid).await.unwrap());
    }
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

/// ADR 0116 A-D4 host death: the mark is lease-checked (a live lease
/// aborts with Conflict; expiry-or-NULL proceeds), orphans with honest
/// prevs, is idempotent, and dead is sticky against heartbeats.
async fn host_lifecycle(ctx: &Ctx) {
    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    let h1 = HostId::new();
    let h2 = HostId::new();
    // h1 registers with NO lease (NULL = expired = markable); h2 with a
    // live one.
    meta.upsert_host(host_record(h1, "conf-h1", now))
        .await
        .unwrap();
    let mut rec2 = host_record(h2, "conf-h2", now);
    rec2.lease_expires_at = Some(now + chrono::Duration::seconds(45));
    meta.upsert_host(rec2).await.unwrap();

    // A live lease aborts the mark with Conflict and changes NOTHING.
    let sid2 = meta.create_session(spec("conf:shielded")).await.unwrap();
    meta.assign_session_host(sid2, Some(h2)).await.unwrap();
    let err = meta.mark_host_dead_if_lease_expired(h2).await.unwrap_err();
    assert!(
        matches!(err, MetaError::Conflict(_)),
        "live lease must abort the mark with Conflict, got {err:?}"
    );
    assert_eq!(meta.host_status(h2).await.unwrap(), Some(HostStatus::Ready));
    assert_eq!(
        meta.get_session(sid2).await.unwrap().status,
        SessionState::Pending,
        "an aborted mark must not touch the host's sessions"
    );

    // Orphaning on an expired (NULL) lease: honest prev states, idempotent.
    let sid = meta.create_session(spec("conf:orphan")).await.unwrap();
    meta.assign_session_host(sid, Some(h1)).await.unwrap();
    let affected = meta.mark_host_dead_if_lease_expired(h1).await.unwrap();
    assert_eq!(affected, vec![(sid, SessionState::Pending)]);
    assert_eq!(
        meta.get_session(sid).await.unwrap().status,
        SessionState::HostLost
    );
    assert!(meta
        .mark_host_dead_if_lease_expired(h1)
        .await
        .unwrap()
        .is_empty());
    // Unknown host: empty, not an error (idempotent with a raced delete).
    assert!(meta
        .mark_host_dead_if_lease_expired(HostId::new())
        .await
        .unwrap()
        .is_empty());

    // Once h2's lease expires, the mark proceeds.
    ctx.clock.advance(Duration::from_secs(46));
    let affected = meta.mark_host_dead_if_lease_expired(h2).await.unwrap();
    assert_eq!(affected, vec![(sid2, SessionState::Pending)]);

    // Dead is sticky against a Ready-claiming heartbeat.
    let hb = HostHeartbeat {
        status: HostStatus::Ready,
        capacity: host_record(h1, "conf-h1", now).capacity,
        utilization: Default::default(),
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        sandbox_bundles: Vec::new(),
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
        lease_renew_until: None,
    };
    meta.touch_host_heartbeat(h1, hb).await.unwrap();
    assert_eq!(meta.host_status(h1).await.unwrap(), Some(HostStatus::Dead));

    // ADR 0112 (D4 conformance for the util_committed_swap_mib column):
    // the heartbeat's committed-swap term round-trips into the host
    // read on BOTH stores — placement's floor math depends on it.
    // A fresh host: h1 and h2 are both dead by this point.
    let h3 = HostId::new();
    let t = ctx.clock.now_utc();
    meta.upsert_host(host_record(h3, "conf-h3", t))
        .await
        .unwrap();
    let util = engram_core::types::host::HostUtilization {
        disk_total_mib: 400_000,
        disk_used_mib: 100_000,
        committed_swap_mib: 12_288,
        ..Default::default()
    };
    let hb2 = HostHeartbeat {
        status: HostStatus::Ready,
        capacity: host_record(h3, "conf-h3", t).capacity,
        utilization: util,
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        sandbox_bundles: Vec::new(),
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
        lease_renew_until: None,
    };
    meta.touch_host_heartbeat(h3, hb2).await.unwrap();
    let h3_row = meta
        .list_active_hosts()
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.id == h3)
        .expect("h3 active");
    assert_eq!(h3_row.utilization.committed_swap_mib, 12_288);
    assert_eq!(h3_row.utilization.disk_used_mib, 100_000);
}

/// ADR 0035 amendment D2 (D4 conformance for `bundle_pin_set` + the
/// `hosts.sandbox_bundles` column): the pin union covers snapshot rows
/// ∪ live hosts' per-sandbox attachments ∪ live hosts' bake stamps,
/// dedupes across legs, sorts by `(drive_id, sha256)`, and drops the
/// host legs when the host dies. The sandbox-attachment leg is the
/// 2026-08-10 chain_poisoned fix: a running-but-unsnapshotted
/// sandbox's generations must pin against sweep + GC.
async fn bundle_pin_set_union(ctx: &Ctx) {
    use engram_core::types::sandbox::{AuxBundleRef, SandboxAuxBundles};
    let meta = &ctx.meta;
    let now = ctx.clock.now_utc();
    assert!(meta.bundle_pin_set().await.unwrap().is_empty());

    let aux = |drive: &str, sha: &str| AuxBundleRef {
        drive_id: drive.to_string(),
        sha256: sha.to_string(),
    };
    let snap_pin = aux("skills", "aaa1");
    let stamp_pin = aux("dyn_0", "bbb2");
    let attach_pin = aux("browser", "ccc3");

    // Snapshot leg.
    let sid = meta.create_session(spec("conf:pins")).await.unwrap();
    let s1 = SnapshotId::new();
    let mut snap = snapshot(s1, sid, now, true);
    snap.aux_bundles = vec![snap_pin.clone()];
    assert!(meta.record_snapshot(snap).await.unwrap());

    // Host legs: a live host's stamp + a RUNNING sandbox's attachments
    // (which duplicate the snapshot pin to prove cross-leg dedup).
    let h1 = HostId::new();
    meta.upsert_host(host_record(h1, "conf-pins-h1", now))
        .await
        .unwrap();
    let mut hb = heartbeat_fixture();
    hb.current_bundles = vec![stamp_pin.clone()];
    hb.sandbox_bundles = vec![SandboxAuxBundles {
        sandbox_id: engram_core::SandboxId::new(),
        bundles: vec![attach_pin.clone(), snap_pin.clone()],
    }];
    meta.touch_host_heartbeat(h1, hb).await.unwrap();

    let pins = meta.bundle_pin_set().await.unwrap();
    assert_eq!(
        pins,
        vec![attach_pin.clone(), stamp_pin.clone(), snap_pin.clone()],
        "union of all three legs, deduped, sorted by (drive_id, sha256)"
    );

    // A dead host's legs drop out; the snapshot pin outlives it.
    meta.set_host_status(h1, HostStatus::Dead).await.unwrap();
    assert_eq!(meta.bundle_pin_set().await.unwrap(), vec![snap_pin]);
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
        // 0108: the production fused Pending→Created bind.
        meta.transition_session_created(sid, sb).await.unwrap();
        sb
    };

    let m_a = uuid::Uuid::from_u128(0xA);
    let m_b = uuid::Uuid::from_u128(0xB);
    let m_c = uuid::Uuid::from_u128(0xC);
    let m_d = uuid::Uuid::from_u128(0xD);
    let m_e = uuid::Uuid::from_u128(0xE);
    let m_f = uuid::Uuid::from_u128(0xF);
    let m_g = uuid::Uuid::from_u128(0x10);

    // Active + sandbox: live A@5 vs recoverable snap A@3 → same id,
    // live is newer → A@5.
    let s_active = meta
        .create_session(spec("conf:rehydrate-active"))
        .await
        .unwrap();
    let sb_active = bind(s_active, host).await;
    meta.transition_session(s_active, SessionState::Active, BindingDisposition::Retain)
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
    meta.transition_session(s_parked, SessionState::Active, BindingDisposition::Retain)
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
    meta.transition_session(s_parked, SessionState::Evicting, BindingDisposition::Retain)
        .await
        .unwrap();

    // ADR 0101 C — a session at the `parked` STATUS (paused in place,
    // sandbox bound). THE 2026-07-21 regression (session 61a03b7e): the
    // hand-rolled SQL status list missed 'parked', the parked survivor
    // vanished from the register-time rehydrate list after a host-agent
    // roll, and the quarantine ladder destroyed its healthy paused VM
    // (93 events rewound). Live G@3, no snapshots → G@3.
    let s_c_parked = meta
        .create_session(spec("conf:rehydrate-parked-status"))
        .await
        .unwrap();
    let sb_c_parked = bind(s_c_parked, host).await;
    meta.transition_session(s_c_parked, SessionState::Active, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.update_live_disk_manifest(s_c_parked, sb_c_parked, mref(m_g, 3))
        .await
        .unwrap();
    meta.transition_session(
        s_c_parked,
        SessionState::Evicting,
        BindingDisposition::Retain,
    )
    .await
    .unwrap();
    meta.transition_session(s_c_parked, SessionState::Parked, BindingDisposition::Retain)
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
    meta.transition_session(s_idle, SessionState::Active, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.transition_session(s_idle, SessionState::Idle, BindingDisposition::Detach)
        .await
        .unwrap();
    meta.assign_session_sandbox(s_idle, None).await.unwrap();

    // Active on ANOTHER host → excluded from this host's list.
    let s_elsewhere = meta
        .create_session(spec("conf:rehydrate-elsewhere"))
        .await
        .unwrap();
    bind(s_elsewhere, other_host).await;
    meta.transition_session(
        s_elsewhere,
        SessionState::Active,
        BindingDisposition::Retain,
    )
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
        (s_c_parked, sb_c_parked, Some(mref(m_g, 3))),
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
    // 0108: the production fused Pending→Created bind.
    meta.transition_session_created(sid, sb).await.unwrap();
    meta.transition_session(sid, SessionState::Active, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.transition_session(sid, SessionState::Evicting, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.transition_session(sid, SessionState::Parked, BindingDisposition::Retain)
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

    // A parked VM is RESIDENT: paused in place, sandbox bound, memory
    // held. Every residency-derived surface must see it (the 2026-07-21
    // 61a03b7e incident: the PG literal lists missed 'parked', so the
    // parked survivor was invisible to the register-time rehydrate list
    // and its paused VM was destroyed after a host-agent roll).
    assert!(
        meta.list_active_sessions()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == sid),
        "a parked session is in the non-terminal (active) set"
    );
    assert!(
        meta.list_resident_sandboxes_on_host_with_disk_manifest(host)
            .await
            .unwrap()
            .iter()
            .any(|(s, sb2, _)| *s == sid && *sb2 == sb),
        "a parked survivor is in the register-time rehydrate list"
    );
    assert!(
        meta.per_host_reserved().await.unwrap().contains_key(&host),
        "a parked session still counts toward its host's reservation aggregate"
    );
    // The reconcile pass's assignment listing (audit finding 3): a
    // vanished parked VM must be strike-eligible, so the parked triple
    // must be listed, status included.
    assert_eq!(
        meta.list_resident_sandbox_assignments_on_host(host)
            .await
            .unwrap(),
        vec![(sid, sb, SessionState::Parked)],
        "a parked survivor is in the reconcile assignment list"
    );
    // The admin-drain listing (audit finding 4): a host holding only a
    // parked VM must not report an empty drain work-list.
    let drain_rows = meta
        .list_resident_assignments_with_budgets_on_host(host)
        .await
        .unwrap();
    assert_eq!(drain_rows.len(), 1, "parked survivor is drain-visible");
    assert_eq!(drain_rows[0].session_id, sid);
    assert_eq!(drain_rows[0].status, SessionState::Parked);
    assert!(
        matches!(
            meta.delete_host(host).await.unwrap(),
            engram_core::types::session::DeleteHostOutcome::SessionsBound(1)
        ),
        "a parked session blocks host deletion"
    );

    // Descend: Parked → Evicting (the explicit nomination edge).
    meta.transition_session(sid, SessionState::Evicting, BindingDisposition::Retain)
        .await
        .unwrap();
    assert!(meta.list_parked_sessions().await.unwrap().is_empty());

    let snap_id = SnapshotId::new();
    // The atomic settle facts (see the trait doc: caller-side emits
    // after a CAS-once settle had a crash window of permanent loss).
    let settle_events = vec![
        ("evicted".to_string(), serde_json::json!({"at": "t"})),
        (
            "status_changed".to_string(),
            serde_json::json!({"to": "idle"}),
        ),
    ];
    let ev_floor = meta
        .append_session_event(sid, "status_changed", serde_json::json!({"probe": true}))
        .await
        .unwrap();
    // 1. No row yet → no settle.
    assert!(
        meta.settle_evicted_session_idle(sid, sb, snap_id, &settle_events)
            .await
            .unwrap()
            .is_none(),
        "no settle before the snapshot row exists"
    );
    // 2. A NON-recoverable row → no settle.
    assert!(meta
        .record_snapshot(snapshot(snap_id, sid, now, false))
        .await
        .unwrap());
    assert!(
        meta.settle_evicted_session_idle(sid, sb, snap_id, &settle_events)
            .await
            .unwrap()
            .is_none(),
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
        meta.settle_evicted_session_idle(sid, sb, other_snap, &settle_events)
            .await
            .unwrap()
            .is_none(),
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
        meta.settle_evicted_session_idle(
            sid,
            engram_core::SandboxId::new(),
            good_snap,
            &settle_events
        )
        .await
        .unwrap()
        .is_none(),
        "a stale advert against a rebound sandbox must not settle"
    );
    // 5. The exact triple → settle: idle + detached (host kept for
    //    resume affinity).
    let idxs = meta
        .settle_evicted_session_idle(sid, sb, good_snap, &settle_events)
        .await
        .unwrap()
        .expect("the exact triple settles");
    assert_eq!(
        idxs,
        vec![ev_floor + 1, ev_floor + 2],
        "settle facts land atomically with contiguous indices",
    );
    let s = meta.get_session(sid).await.unwrap();
    assert_eq!(s.status, SessionState::Idle);
    assert_eq!(s.sandbox_id, None, "the settle detaches the sandbox");
    assert_eq!(s.host_id, Some(host), "host affinity preserved");
    // ADR 0116 A6: the settle entombs the released binding in the SAME
    // transaction — normally the next heartbeat acks it by absence
    // instantly; after a host crash it destroys the survivor from an
    // explicit fact.
    assert!(
        meta.sandbox_tombstones_for_host(host)
            .await
            .unwrap()
            .contains(&sb),
        "the settle writes the released binding'''s tombstone in-tx"
    );
    // 6. Idempotent: a re-advert's second settle is a clean no-op.
    assert!(
        meta.settle_evicted_session_idle(sid, sb, good_snap, &settle_events)
            .await
            .unwrap()
            .is_none(),
        "an already-settled session no-ops"
    );
    // Every no-op arm above (and the idempotent re-settle) appended
    // NOTHING; only the one successful settle's two facts landed.
    let landed = meta
        .list_session_events_since(sid, ev_floor, 100)
        .await
        .unwrap();
    assert_eq!(
        landed.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        vec!["evicted", "status_changed"],
        "exactly the settle's facts, exactly once: {landed:?}",
    );

    // Parked → HostLost is the host-death edge (never Idle: the parked
    // RAM died with the host).
    let sid2 = meta.create_session(spec("conf:parked-lost")).await.unwrap();
    meta.assign_session_host(sid2, Some(host)).await.unwrap();
    // 0108: the production fused Pending→Created bind.
    meta.transition_session_created(sid2, engram_core::SandboxId::new())
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::Active, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::Evicting, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::Parked, BindingDisposition::Retain)
        .await
        .unwrap();
    meta.transition_session(sid2, SessionState::HostLost, BindingDisposition::Retain)
        .await
        .unwrap();
    assert_eq!(
        meta.get_session(sid2).await.unwrap().status,
        SessionState::HostLost
    );
}

/// ADR 0106: OAuth rows have identical subject isolation, version/CAS,
/// revocation, owner fencing, and deterministic cleanup in PG and SimMeta.
async fn oauth_credential_and_flow(ctx: &Ctx) {
    use engram_core::types::oauth::{
        NewSealedOAuthCredential, OAuthAccountMetadata, OAuthCredentialKey, OAuthFlow,
        OAuthFlowStatus, OAuthSubjectKind,
    };

    let key = OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::User,
        subject_id: "user-a".into(),
        provider: "openai-codex".into(),
    };
    let candidate = |account: &str, byte: u8| NewSealedOAuthCredential {
        key: key.clone(),
        wrapped_dek: vec![byte; 32],
        nonce: vec![byte; 12],
        ciphertext: vec![byte; 8],
        key_id: "test-kek".into(),
        metadata: OAuthAccountMetadata {
            account_id: account.into(),
            display_name: Some("Test User".into()),
            plan_type: Some("personal".into()),
            workspace_id: None,
            workspace_name: None,
        },
        expires_at: None,
    };

    let first = ctx
        .meta
        .put_oauth_credential(candidate("acct-a", 1), None)
        .await
        .unwrap();
    assert_eq!(first.version, 1);
    assert!(first.revoked_at.is_none());
    assert!(matches!(
        ctx.meta
            .put_oauth_credential(candidate("acct-a", 2), None)
            .await,
        Err(MetaError::Conflict(_))
    ));
    let second = ctx
        .meta
        .put_oauth_credential(candidate("acct-a", 2), Some(1))
        .await
        .unwrap();
    assert_eq!(second.version, 2);
    assert!(matches!(
        ctx.meta
            .put_oauth_credential(candidate("acct-a", 3), Some(1))
            .await,
        Err(MetaError::Conflict(_))
    ));

    let other_subject = ctx
        .meta
        .list_oauth_credentials(OAuthSubjectKind::User, Some("user-b"))
        .await
        .unwrap();
    assert!(
        other_subject.is_empty(),
        "subjects cannot enumerate one another"
    );
    let listed = ctx
        .meta
        .list_oauth_credentials(OAuthSubjectKind::User, Some("user-a"))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, key);

    let revoked = ctx
        .meta
        .revoke_oauth_credential(&key, second.version)
        .await
        .unwrap();
    assert_eq!(revoked.version, 3);
    assert!(revoked.revoked_at.is_some());
    assert!(matches!(
        ctx.meta.revoke_oauth_credential(&key, second.version).await,
        Err(MetaError::Conflict(_))
    ));
    let reconnected = ctx
        .meta
        .put_oauth_credential(candidate("acct-a", 4), Some(revoked.version))
        .await
        .unwrap();
    assert_eq!(reconnected.version, 4);
    assert!(reconnected.revoked_at.is_none());

    let now = ctx.clock.now_utc();
    let flow_id = uuid::Uuid::parse_str("10600000-0000-4000-8000-000000000001").unwrap();
    let flow = OAuthFlow {
        id: flow_id,
        key: key.clone(),
        owner_replica: "replica-a".into(),
        lease_expires_at: now + chrono::Duration::seconds(5),
        expires_at: now + chrono::Duration::seconds(30),
        status: OAuthFlowStatus::Pending,
        error_code: None,
        created_at: now,
        updated_at: now,
    };
    ctx.meta.create_oauth_flow(flow.clone()).await.unwrap();
    assert!(matches!(
        ctx.meta.create_oauth_flow(flow).await,
        Err(MetaError::Conflict(_))
    ));
    assert!(matches!(
        ctx.meta
            .finish_oauth_flow(flow_id, "replica-b", OAuthFlowStatus::Cancelled, None)
            .await,
        Err(MetaError::Conflict(_))
    ));
    assert!(matches!(
        ctx.meta
            .renew_oauth_flow_lease(flow_id, "replica-b", now + chrono::Duration::seconds(10),)
            .await,
        Err(MetaError::Conflict(_))
    ));
    ctx.meta
        .renew_oauth_flow_lease(flow_id, "replica-a", now + chrono::Duration::seconds(10))
        .await
        .unwrap();
    ctx.clock.advance(Duration::from_secs(6));
    assert_eq!(
        ctx.meta
            .cleanup_oauth_flows(
                ctx.clock.now_utc(),
                ctx.clock.now_utc() - chrono::Duration::hours(1),
            )
            .await
            .unwrap(),
        0,
        "a renewed owner lease stays pending"
    );
    ctx.clock.advance(Duration::from_secs(5));
    let cleanup_now = ctx.clock.now_utc();
    assert_eq!(
        ctx.meta
            .cleanup_oauth_flows(cleanup_now, cleanup_now - chrono::Duration::hours(1))
            .await
            .unwrap(),
        1
    );
    let lost = ctx.meta.get_oauth_flow(flow_id).await.unwrap().unwrap();
    assert_eq!(lost.status, OAuthFlowStatus::OwnerLost);
    assert_eq!(lost.error_code.as_deref(), Some("owner_lost"));

    let expires_id = uuid::Uuid::parse_str("10600000-0000-4000-8000-000000000002").unwrap();
    let expires_now = ctx.clock.now_utc();
    ctx.meta
        .create_oauth_flow(OAuthFlow {
            id: expires_id,
            key: key.clone(),
            owner_replica: "replica-a".into(),
            lease_expires_at: expires_now + chrono::Duration::seconds(30),
            expires_at: expires_now + chrono::Duration::seconds(5),
            status: OAuthFlowStatus::Pending,
            error_code: None,
            created_at: expires_now,
            updated_at: expires_now,
        })
        .await
        .unwrap();
    ctx.clock.advance(Duration::from_secs(6));
    let expires_cleanup = ctx.clock.now_utc();
    assert_eq!(
        ctx.meta
            .cleanup_oauth_flows(
                expires_cleanup,
                expires_cleanup - chrono::Duration::hours(1),
            )
            .await
            .unwrap(),
        1
    );
    let expired = ctx.meta.get_oauth_flow(expires_id).await.unwrap().unwrap();
    assert_eq!(expired.status, OAuthFlowStatus::Expired);
    assert_eq!(expired.error_code.as_deref(), Some("flow_expired"));

    ctx.clock.advance(Duration::from_secs(3601));
    let delete_now = ctx.clock.now_utc();
    assert_eq!(
        ctx.meta
            .cleanup_oauth_flows(delete_now, delete_now - chrono::Duration::hours(1))
            .await
            .unwrap(),
        2
    );
    assert!(ctx.meta.get_oauth_flow(flow_id).await.unwrap().is_none());
    assert!(ctx.meta.get_oauth_flow(expires_id).await.unwrap().is_none());
}

/// ADR 0115: the `user_connector` subject kind round-trips through put /
/// list / revoke / flow creation identically in PG and SimMeta. Against PG
/// this also exercises the widened subject-kind CHECK constraints
/// (migration 0114) — without them every write here fails.
async fn user_connector_subject_kind(ctx: &Ctx) {
    use engram_core::types::oauth::{
        NewSealedOAuthCredential, OAuthAccountMetadata, OAuthCredentialKey, OAuthFlow,
        OAuthFlowStatus, OAuthSubjectKind,
    };

    let key = OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::UserConnector,
        subject_id: "user-a".into(),
        provider: "linear".into(),
    };
    let row = ctx
        .meta
        .put_oauth_credential(
            NewSealedOAuthCredential {
                key: key.clone(),
                wrapped_dek: vec![7; 32],
                nonce: vec![7; 12],
                ciphertext: vec![7; 8],
                key_id: "test-kek".into(),
                // A static token seals with no provider-verified identity.
                metadata: OAuthAccountMetadata::default(),
                expires_at: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(row.version, 1);

    // Kind isolation: the same subject id under `user` is a different row.
    assert!(ctx
        .meta
        .list_oauth_credentials(OAuthSubjectKind::User, Some("user-a"))
        .await
        .unwrap()
        .is_empty());
    let listed = ctx
        .meta
        .list_oauth_credentials(OAuthSubjectKind::UserConnector, Some("user-a"))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, key);

    // A no-expiry row never enters the refresh due set.
    let now = ctx.clock.now_utc();
    assert!(ctx
        .meta
        .list_oauth_credentials_due_for_refresh(
            OAuthSubjectKind::UserConnector,
            now,
            now + chrono::Duration::hours(6),
            32,
        )
        .await
        .unwrap()
        .is_empty());

    // Flow rows accept the kind (the oauth_flows CHECK).
    let flow_id = uuid::Uuid::parse_str("11500000-0000-4000-8000-000000000001").unwrap();
    ctx.meta
        .create_oauth_flow(OAuthFlow {
            id: flow_id,
            key: key.clone(),
            owner_replica: "replica-a".into(),
            lease_expires_at: now + chrono::Duration::seconds(30),
            expires_at: now + chrono::Duration::seconds(30),
            status: OAuthFlowStatus::Pending,
            error_code: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    assert_eq!(
        ctx.meta.get_oauth_flow(flow_id).await.unwrap().unwrap().key,
        key
    );

    let revoked = ctx
        .meta
        .revoke_oauth_credential(&key, row.version)
        .await
        .unwrap();
    assert!(revoked.revoked_at.is_some());
}

/// ADR 0106 addendum (connector OAuth): refresh scheduling, advisory claims,
/// the version-fenced broken mark, publish-clears-repair, and the unowned
/// redirect-flow finish behave identically in PG and SimMeta.
async fn oauth_refresh_scheduling(ctx: &Ctx) {
    use engram_core::types::oauth::{
        NewSealedOAuthCredential, OAuthAccountMetadata, OAuthCredentialKey, OAuthCredentialStatus,
        OAuthFlow, OAuthFlowStatus, OAuthSubjectKind,
    };

    let key_for = |provider: &str| OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Connector,
        subject_id: "conn-default".into(),
        provider: provider.into(),
    };
    let candidate = |key: &OAuthCredentialKey,
                     expires_at: Option<chrono::DateTime<chrono::Utc>>| {
        NewSealedOAuthCredential {
            key: key.clone(),
            wrapped_dek: vec![7; 32],
            nonce: vec![7; 12],
            ciphertext: vec![7; 8],
            key_id: "test-kek".into(),
            metadata: OAuthAccountMetadata {
                account_id: "workspace-1".into(),
                display_name: Some("Workspace".into()),
                plan_type: None,
                workspace_id: Some("workspace-1".into()),
                workspace_name: Some("Acme".into()),
            },
            expires_at,
        }
    };

    let now = ctx.clock.now_utc();
    let linear = key_for("linear");
    let slack = key_for("slack");
    let later = key_for("later");
    let user_key = OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::User,
        subject_id: "user-a".into(),
        provider: "openai-codex".into(),
    };

    // An expiring row, a non-expiring row, a row expiring later, and a row of
    // a DIFFERENT subject kind that must never surface in a connector sweep.
    let row = ctx
        .meta
        .put_oauth_credential(
            candidate(&linear, Some(now + chrono::Duration::hours(1))),
            None,
        )
        .await
        .unwrap();
    assert_eq!(row.expires_at, Some(now + chrono::Duration::hours(1)));
    assert_eq!(row.status(now), OAuthCredentialStatus::Connected);
    ctx.meta
        .put_oauth_credential(candidate(&slack, None), None)
        .await
        .unwrap();
    ctx.meta
        .put_oauth_credential(
            candidate(&later, Some(now + chrono::Duration::hours(2))),
            None,
        )
        .await
        .unwrap();
    ctx.meta
        .put_oauth_credential(
            candidate(&user_key, Some(now + chrono::Duration::hours(1))),
            None,
        )
        .await
        .unwrap();

    let horizon = now + chrono::Duration::hours(6);
    let due = ctx
        .meta
        .list_oauth_credentials_due_for_refresh(OAuthSubjectKind::Connector, now, horizon, 10)
        .await
        .unwrap();
    assert_eq!(
        due.iter()
            .map(|r| r.key.provider.as_str())
            .collect::<Vec<_>>(),
        vec!["linear", "later"],
        "expiring connector rows only, ordered by expiry; non-expiring and \
         other-kind rows never surface"
    );
    let limited = ctx
        .meta
        .list_oauth_credentials_due_for_refresh(OAuthSubjectKind::Connector, now, horizon, 1)
        .await
        .unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].key.provider, "linear");

    // Kind-wide listing (subject_id = None): every connector credential in one
    // call — the status surface's read — and never another kind's rows.
    let all_connector = ctx
        .meta
        .list_oauth_credentials(OAuthSubjectKind::Connector, None)
        .await
        .unwrap();
    assert_eq!(
        all_connector
            .iter()
            .map(|r| r.key.provider.as_str())
            .collect::<Vec<_>>(),
        vec!["later", "linear", "slack"],
        "ordered by subject then provider; the user-kind row never surfaces"
    );

    // Same-second expiries: the (expires_at, subject_id, provider) tie-break
    // must pick the SAME subset under LIMIT on both stores. "aaa-first" ties
    // with "linear" on expiry but sorts ahead by subject id.
    let tied = OAuthCredentialKey {
        subject_kind: OAuthSubjectKind::Connector,
        subject_id: "aaa-first".into(),
        provider: "zzz".into(),
    };
    ctx.meta
        .put_oauth_credential(
            candidate(&tied, Some(now + chrono::Duration::hours(1))),
            None,
        )
        .await
        .unwrap();
    let tie_limited = ctx
        .meta
        .list_oauth_credentials_due_for_refresh(OAuthSubjectKind::Connector, now, horizon, 2)
        .await
        .unwrap();
    assert_eq!(
        tie_limited
            .iter()
            .map(|r| (r.key.subject_id.as_str(), r.key.provider.as_str()))
            .collect::<Vec<_>>(),
        vec![("aaa-first", "zzz"), ("conn-default", "linear")],
    );
    ctx.meta
        .revoke_oauth_credential(&tied, tie_limited[0].version)
        .await
        .unwrap();

    // Advisory claim: first caller wins, second loses, a lapsed claim is
    // retaken, and a claimed row leaves the due list until the claim lapses.
    let until = now + chrono::Duration::minutes(5);
    assert!(ctx
        .meta
        .claim_oauth_refresh(&linear, now, until)
        .await
        .unwrap());
    assert!(!ctx
        .meta
        .claim_oauth_refresh(&linear, now, until)
        .await
        .unwrap());
    let due = ctx
        .meta
        .list_oauth_credentials_due_for_refresh(OAuthSubjectKind::Connector, now, horizon, 10)
        .await
        .unwrap();
    assert_eq!(
        due.iter()
            .map(|r| r.key.provider.as_str())
            .collect::<Vec<_>>(),
        vec!["later"]
    );
    ctx.clock.advance(Duration::from_secs(6 * 60));
    let after_lapse = ctx.clock.now_utc();
    assert!(ctx
        .meta
        .claim_oauth_refresh(
            &linear,
            after_lapse,
            after_lapse + chrono::Duration::minutes(5)
        )
        .await
        .unwrap());
    assert!(matches!(
        ctx.meta
            .claim_oauth_refresh(&key_for("absent"), after_lapse, until)
            .await,
        Ok(false)
    ));

    // Version-fenced broken mark: a moved version means a concurrent refresh
    // won — Conflict, reload the winner, never declare the row dead.
    let winner = ctx
        .meta
        .put_oauth_credential(
            candidate(&linear, Some(after_lapse + chrono::Duration::hours(24))),
            Some(row.version),
        )
        .await
        .unwrap();
    assert_eq!(winner.version, row.version + 1);
    assert!(matches!(
        ctx.meta
            .mark_oauth_credential_broken(&linear, row.version, "invalid_grant")
            .await,
        Err(MetaError::Conflict(_))
    ));
    assert!(matches!(
        ctx.meta
            .mark_oauth_credential_broken(&key_for("absent"), 1, "invalid_grant")
            .await,
        Err(MetaError::NotFound)
    ));
    let broken = ctx
        .meta
        .mark_oauth_credential_broken(&linear, winner.version, "invalid_grant")
        .await
        .unwrap();
    assert_eq!(
        broken.version, winner.version,
        "the mark is not a bundle write"
    );
    assert!(broken.broken_at.is_some());
    assert_eq!(broken.broken_reason.as_deref(), Some("invalid_grant"));
    assert_eq!(
        broken.status(ctx.clock.now_utc()),
        OAuthCredentialStatus::Broken
    );
    assert!(matches!(
        ctx.meta
            .mark_oauth_credential_broken(&linear, winner.version, "invalid_grant")
            .await,
        Err(MetaError::Conflict(_))
    ));

    // Broken rows leave the due list AND refuse claims; a successful publish
    // (reconnect) clears the mark and the claim in the same write.
    let broken_now = ctx.clock.now_utc();
    let due = ctx
        .meta
        .list_oauth_credentials_due_for_refresh(
            OAuthSubjectKind::Connector,
            broken_now,
            broken_now + chrono::Duration::hours(48),
            10,
        )
        .await
        .unwrap();
    assert!(!due.iter().any(|r| r.key.provider == "linear"));
    assert!(!ctx
        .meta
        .claim_oauth_refresh(
            &linear,
            broken_now,
            broken_now + chrono::Duration::minutes(5)
        )
        .await
        .unwrap());
    let repaired = ctx
        .meta
        .put_oauth_credential(
            candidate(&linear, Some(broken_now + chrono::Duration::hours(24))),
            Some(broken.version),
        )
        .await
        .unwrap();
    assert!(repaired.broken_at.is_none());
    assert!(repaired.broken_reason.is_none());
    assert_eq!(
        repaired.status(broken_now),
        OAuthCredentialStatus::Connected
    );
    assert!(ctx
        .meta
        .claim_oauth_refresh(
            &linear,
            broken_now,
            broken_now + chrono::Duration::minutes(5)
        )
        .await
        .unwrap());

    // Unowned finish: any replica may complete a pending, unexpired redirect
    // flow; the pending→terminal transition is the fence.
    let flow_now = ctx.clock.now_utc();
    let flow_id = uuid::Uuid::parse_str("10600000-0000-4000-8000-000000000011").unwrap();
    ctx.meta
        .create_oauth_flow(OAuthFlow {
            id: flow_id,
            key: linear.clone(),
            owner_replica: "replica-a".into(),
            lease_expires_at: flow_now + chrono::Duration::minutes(10),
            expires_at: flow_now + chrono::Duration::minutes(10),
            status: OAuthFlowStatus::Pending,
            error_code: None,
            created_at: flow_now,
            updated_at: flow_now,
        })
        .await
        .unwrap();
    assert!(matches!(
        ctx.meta
            .finish_oauth_flow_unowned(flow_id, OAuthFlowStatus::Pending, None)
            .await,
        Err(MetaError::Conflict(_))
    ));
    // The pending flow is findable by key (redirect begin cancels stale
    // attempts through this) and other keys see nothing.
    let pending = ctx.meta.get_pending_oauth_flow(&linear).await.unwrap();
    assert_eq!(pending.map(|f| f.id), Some(flow_id));
    assert!(ctx
        .meta
        .get_pending_oauth_flow(&key_for("absent"))
        .await
        .unwrap()
        .is_none());
    ctx.meta
        .finish_oauth_flow_unowned(flow_id, OAuthFlowStatus::Succeeded, None)
        .await
        .unwrap();
    assert!(
        ctx.meta
            .get_pending_oauth_flow(&linear)
            .await
            .unwrap()
            .is_none(),
        "a finished flow is no longer pending"
    );
    assert!(matches!(
        ctx.meta
            .finish_oauth_flow_unowned(flow_id, OAuthFlowStatus::Cancelled, None)
            .await,
        Err(MetaError::Conflict(_)),
    ));
    assert!(matches!(
        ctx.meta
            .finish_oauth_flow_unowned(
                uuid::Uuid::parse_str("10600000-0000-4000-8000-000000000012").unwrap(),
                OAuthFlowStatus::Succeeded,
                None,
            )
            .await,
        Err(MetaError::NotFound)
    ));

    let expired_id = uuid::Uuid::parse_str("10600000-0000-4000-8000-000000000013").unwrap();
    let expired_now = ctx.clock.now_utc();
    ctx.meta
        .create_oauth_flow(OAuthFlow {
            id: expired_id,
            key: key_for("slack"),
            owner_replica: "replica-a".into(),
            lease_expires_at: expired_now + chrono::Duration::minutes(10),
            expires_at: expired_now + chrono::Duration::seconds(5),
            status: OAuthFlowStatus::Pending,
            error_code: None,
            created_at: expired_now,
            updated_at: expired_now,
        })
        .await
        .unwrap();
    ctx.clock.advance(Duration::from_secs(6));
    assert!(matches!(
        ctx.meta
            .finish_oauth_flow_unowned(expired_id, OAuthFlowStatus::Succeeded, None)
            .await,
        Err(MetaError::Conflict(_)),
    ));
}

conformance!(t_broker_token_flow, super::broker_token_flow);
conformance!(
    t_oauth_credential_and_flow,
    super::oauth_credential_and_flow
);
conformance!(t_oauth_refresh_scheduling, super::oauth_refresh_scheduling);
conformance!(
    t_user_connector_subject_kind,
    super::user_connector_subject_kind
);
conformance!(
    t_parked_lifecycle_and_eviction_settle,
    super::parked_lifecycle_and_eviction_settle
);
conformance!(t_teleport_target_flow, super::teleport_target_flow);
conformance!(t_session_lifecycle, super::session_lifecycle);
conformance!(
    t_binding_disposition_contract,
    super::binding_disposition_contract
);
conformance!(t_session_exec_event_at, super::session_exec_event_at);
conformance!(
    t_session_exec_output_high_water,
    super::session_exec_output_high_water
);
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
conformance!(t_host_binding_lease, super::host_binding_lease);
conformance!(t_sandbox_tombstones, super::sandbox_tombstones);
conformance!(t_ops_pipeline, super::ops_pipeline);
conformance!(t_ops_idempotency, super::ops_idempotency);
conformance!(
    t_op_latest_for_kind_reads_terminal_mints,
    super::op_latest_for_kind_reads_terminal_mints
);
conformance!(t_fenced_transition, super::fenced_transition);
conformance!(
    t_fenced_transition_with_events,
    super::fenced_transition_with_events
);
conformance!(
    t_enqueue_evacuating_resume,
    super::enqueue_evacuating_resume
);
conformance!(t_outbox_flow, super::outbox_flow);
conformance!(t_snapshot_durable_head, super::snapshot_durable_head);
conformance!(t_gc_candidates, super::gc_candidates);
conformance!(t_host_lifecycle, super::host_lifecycle);
conformance!(t_bundle_pin_set_union, super::bundle_pin_set_union);
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
        oauth_binding: None,
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
/// ADR 0105: the event stream WALKS the log in pages, feeding each read's
/// last idx back as the next `since`. That walk is only gap-free and
/// dup-free if `list_session_events_since` treats the page boundary
/// identically in both stores — a divergence between the sim's
/// `.take(limit)` and SQL `LIMIT`/`ORDER BY` would silently break the walk
/// in exactly one of them, which is the failure a single-store test cannot
/// see.
///
/// The store method itself is unchanged by that ADR, so D4 does not compel
/// this scenario; the new paging DEPENDS on the boundary agreeing, so it
/// earns one anyway.
async fn list_session_events_pages_without_gap_or_dup(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta.create_session(spec("conf:paging")).await.unwrap();

    // Deliberately not a multiple of the page size, so the walk ends on a
    // SHORT page (the ordinary case) rather than an exactly-full one.
    const TOTAL: i64 = 25;
    const PAGE: i64 = 10;
    for n in 0..TOTAL {
        meta.append_session_event(id, "agent_message", serde_json::json!({ "n": n }))
            .await
            .unwrap();
    }

    // Walk exactly the way the stream does: read above the cursor, then
    // advance the cursor to the last idx returned.
    let mut cursor = -1i64;
    let mut walked = Vec::new();
    let mut reads = 0;
    loop {
        let page = meta
            .list_session_events_since(id, cursor, PAGE)
            .await
            .unwrap();
        reads += 1;
        assert!(
            page.len() as i64 <= PAGE,
            "a page must never exceed the requested limit"
        );
        // Ascending order is what makes the cursor monotonic; assert it
        // rather than trusting it.
        assert!(
            page.windows(2).all(|w| w[0].idx < w[1].idx),
            "page must be in strictly ascending idx order"
        );
        let full = page.len() as i64 == PAGE;
        for ev in &page {
            assert!(
                ev.idx > cursor,
                "page must contain only events above `since`"
            );
            walked.push(ev.idx);
        }
        if let Some(last) = page.last() {
            cursor = last.idx;
        }
        if !full {
            break;
        }
        assert!(reads < 10, "walk failed to terminate");
    }

    let expected: Vec<i64> = (0..TOTAL).collect();
    assert_eq!(
        walked, expected,
        "the walk reconstructs the whole log exactly once"
    );
    // We do NOT assert an exact read count. The number of reads is an
    // implementation detail of the page size. The behavior that matters is
    // above: no gap, no duplicate, and the walk terminates.

    // At the tail, a further read returns nothing and must NOT rewind the
    // cursor — the stream relies on this to stop walking.
    let past_tail = meta
        .list_session_events_since(id, cursor, PAGE)
        .await
        .unwrap();
    assert!(past_tail.is_empty(), "no events past the tail");

    // An exactly-full final page must also terminate: walking from a
    // cursor that leaves precisely PAGE events behind takes one full read
    // plus one empty one.
    let boundary_cursor = TOTAL - 1 - PAGE;
    let full_page = meta
        .list_session_events_since(id, boundary_cursor, PAGE)
        .await
        .unwrap();
    assert_eq!(
        full_page.len() as i64,
        PAGE,
        "boundary page is exactly full"
    );
    let after_full = meta
        .list_session_events_since(id, full_page.last().unwrap().idx, PAGE)
        .await
        .unwrap();
    assert!(
        after_full.is_empty(),
        "an exactly-full final page is followed by an empty read, not a repeat"
    );
}

// ---------------------------------------------------------------------------
// Windowed transcript reads (`list_session_events_window`)
//
// D4 obligation for the new store method. The web transcript opens on the
// LAST page and backfills upward, so a backward page that overlaps or skips
// at the boundary duplicates or loses transcript lines — a defect only a
// two-store test can catch, because sim `.rev().take()` and SQL
// `ORDER BY idx DESC LIMIT` are different code with the same contract.
// ---------------------------------------------------------------------------

/// Read the whole window with the direction/filters spelled out, so each
/// scenario below reads as the property it asserts.
async fn window(
    ctx: &Ctx,
    id: SessionId,
    cursor: EventCursor,
    limit: i64,
    kinds: &[&str],
    tool_names: &[&str],
) -> Vec<engram_core::types::PersistedEvent> {
    let kinds: Vec<String> = kinds.iter().map(|s| s.to_string()).collect();
    let tools: Vec<String> = tool_names.iter().map(|s| s.to_string()).collect();
    ctx.meta
        .list_session_events_window(id, cursor, limit, &kinds, &tools)
        .await
        .unwrap()
}

fn idxs(events: &[engram_core::types::PersistedEvent]) -> Vec<i64> {
    events.iter().map(|e| e.idx).collect()
}

/// Every page, both directions, arrives in ascending idx order — the
/// property that lets one renderer draw a forward and a backward page.
fn assert_ascending(page: &[engram_core::types::PersistedEvent]) {
    assert!(
        page.windows(2).all(|w| w[0].idx < w[1].idx),
        "a page must be in strictly ascending idx order, got {:?}",
        idxs(page)
    );
}

/// (a) The backward walk is the forward walk. Paging backward from the tail
/// and concatenating the pages yields the SAME sequence as paging forward
/// from -1, and the two directions meet exactly at a shared boundary:
/// `Before(k)` ++ `After(k - 1)` is the whole log, no gap, no duplicate.
async fn events_backward_walk_equals_forward_walk(ctx: &Ctx) {
    let id = ctx
        .meta
        .create_session(spec("conf:window-back"))
        .await
        .unwrap();

    // Not a multiple of the page size, so both walks end on a SHORT page.
    const TOTAL: i64 = 25;
    const PAGE: i64 = 10;
    for n in 0..TOTAL {
        ctx.meta
            .append_session_event(id, "agent_message", serde_json::json!({ "n": n }))
            .await
            .unwrap();
    }

    let mut forward = Vec::new();
    let mut cursor = -1i64;
    loop {
        let page = window(ctx, id, EventCursor::After(cursor), PAGE, &[], &[]).await;
        if page.is_empty() {
            break;
        }
        assert!(page.len() as i64 <= PAGE, "page must respect the limit");
        assert_ascending(&page);
        cursor = page.last().unwrap().idx;
        forward.extend(idxs(&page));
    }
    assert_eq!(forward, (0..TOTAL).collect::<Vec<_>>());

    // Backward: anchor at "below every idx", then re-anchor on the FIRST
    // idx of the page just read (the transcript's backfill loop).
    let mut backward: Vec<i64> = Vec::new();
    let mut anchor = i64::MAX;
    let mut reads = 0;
    loop {
        let page = window(ctx, id, EventCursor::Before(anchor), PAGE, &[], &[]).await;
        reads += 1;
        assert!(reads < 10, "backward walk failed to terminate");
        if page.is_empty() {
            break;
        }
        assert!(page.len() as i64 <= PAGE, "page must respect the limit");
        assert_ascending(&page);
        assert!(
            page.iter().all(|e| e.idx < anchor),
            "a backward page must stay strictly below its anchor"
        );
        anchor = page.first().unwrap().idx;
        let mut head = idxs(&page);
        head.extend(std::mem::take(&mut backward));
        backward = head;
    }
    assert_eq!(
        backward, forward,
        "the backward walk reconstructs the same log as the forward walk"
    );

    // The boundary itself: one backward page and one forward page taken at
    // the same idx tile the log exactly once.
    let boundary = TOTAL / 2;
    let below = window(ctx, id, EventCursor::Before(boundary), TOTAL, &[], &[]).await;
    let above = window(ctx, id, EventCursor::After(boundary - 1), TOTAL, &[], &[]).await;
    let mut joined = idxs(&below);
    joined.extend(idxs(&above));
    assert_eq!(
        joined,
        (0..TOTAL).collect::<Vec<_>>(),
        "Before(k) ++ After(k-1) is contiguous: no gap, no duplicate"
    );
}

/// Append the mixed transcript the filter scenarios read: three tools, all
/// four tool kinds, a non-tool kind, and one malformed tool event with no
/// name field at all.
async fn seed_mixed_transcript(ctx: &Ctx, id: SessionId) {
    let ev: &[(&str, serde_json::Value)] = &[
        ("agent_message", serde_json::json!({"text": "planning"})),
        (
            "tool_call_requested",
            serde_json::json!({"name": "Edit", "tool_call_id": "t1"}),
        ),
        (
            "tool_call_started",
            serde_json::json!({"tool_name": "Edit", "tool_call_id": "t1"}),
        ),
        (
            "tool_call_completed",
            serde_json::json!({"tool_name": "Edit", "tool_call_id": "t1"}),
        ),
        (
            "tool_result_submitted",
            serde_json::json!({"tool_call_id": "t1"}),
        ),
        ("status_changed", serde_json::json!({"to": "running"})),
        (
            "tool_call_requested",
            serde_json::json!({"name": "Bash", "tool_call_id": "t2"}),
        ),
        (
            "tool_call_started",
            serde_json::json!({"tool_name": "Bash", "tool_call_id": "t2"}),
        ),
        (
            "tool_call_completed",
            serde_json::json!({"tool_name": "Bash", "tool_call_id": "t2"}),
        ),
        (
            "tool_result_submitted",
            serde_json::json!({"tool_call_id": "t2"}),
        ),
        ("agent_message", serde_json::json!({"text": "done"})),
        // Malformed on purpose: a tool kind whose payload carries NO name.
        // The contract says "no name, no filter", so it must pass through.
        (
            "tool_call_started",
            serde_json::json!({"tool_call_id": "t3"}),
        ),
    ];
    for (kind, payload) in ev {
        ctx.meta
            .append_session_event(id, kind, payload.clone())
            .await
            .unwrap();
    }
}

/// (b) A filtered read is the unfiltered read, filtered. The store may run
/// the predicate in SQL, but it must not change WHICH events a caller sees —
/// only how many bytes cross the wire to get them.
async fn events_kind_filter_equals_in_memory_filter(ctx: &Ctx) {
    let id = ctx
        .meta
        .create_session(spec("conf:window-kinds"))
        .await
        .unwrap();
    seed_mixed_transcript(ctx, id).await;

    let all = window(ctx, id, EventCursor::After(-1), 1000, &[], &[]).await;
    for selection in [
        vec!["agent_message"],
        vec!["tool_call_requested"],
        vec!["agent_message", "status_changed"],
        // A kind nobody appended: an empty page, NOT "every kind".
        vec!["no_such_kind"],
    ] {
        let filtered = window(ctx, id, EventCursor::After(-1), 1000, &selection, &[]).await;
        let expected: Vec<i64> = all
            .iter()
            .filter(|e| selection.contains(&e.kind.as_str()))
            .map(|e| e.idx)
            .collect();
        assert_eq!(
            idxs(&filtered),
            expected,
            "kinds={selection:?} must select exactly the in-memory filter"
        );
        assert_ascending(&filtered);

        // The same must hold backward — the filter runs INSIDE the
        // newest-first subquery, so a broken one silently returns the
        // newest N events of the WRONG kind.
        let back = window(
            ctx,
            id,
            EventCursor::Before(i64::MAX),
            1000,
            &selection,
            &[],
        )
        .await;
        assert_eq!(idxs(&back), expected, "backward kinds={selection:?}");
    }

    // The empty list is "every kind", never "no kind".
    assert_eq!(
        idxs(&window(ctx, id, EventCursor::After(-1), 1000, &[], &[]).await),
        idxs(&all),
        "an empty kinds list keeps every kind"
    );
}

/// (c) `tool_names` filters the kinds that CARRY a name
/// (`tool_call_requested` reads `name`, started/completed read `tool_name`)
/// and passes through every kind that does not — `tool_result_submitted`,
/// which holds only a `tool_call_id`, plus every non-tool kind the caller
/// selected. This is the documented rule; it is asserted here because a
/// store that "helpfully" drops the un-named kinds loses tool results the
/// caller explicitly asked for.
async fn events_tool_name_filter_spares_nameless_kinds(ctx: &Ctx) {
    let id = ctx
        .meta
        .create_session(spec("conf:window-tools"))
        .await
        .unwrap();
    seed_mixed_transcript(ctx, id).await;

    let all = window(ctx, id, EventCursor::After(-1), 1000, &[], &[]).await;
    let expect_for = |tools: &[&str]| -> Vec<i64> {
        all.iter()
            .filter(|e| match e.tool_name() {
                Some(name) => tools.contains(&name),
                // No name field (or a malformed payload): never filtered.
                None => true,
            })
            .map(|e| e.idx)
            .collect()
    };

    for tools in [
        vec!["Edit"],
        vec!["Bash"],
        vec!["Edit", "Bash"],
        vec!["Grep"],
    ] {
        let got = window(ctx, id, EventCursor::After(-1), 1000, &[], &tools).await;
        assert_eq!(
            idxs(&got),
            expect_for(&tools),
            "tool_names={tools:?} keeps that tool plus every nameless kind"
        );
        assert_ascending(&got);
        let back = window(ctx, id, EventCursor::Before(i64::MAX), 1000, &[], &tools).await;
        assert_eq!(
            idxs(&back),
            expect_for(&tools),
            "backward tool_names={tools:?}"
        );
    }

    // Named checks on the four tool kinds + a non-tool kind, so a change to
    // the kind→field map fails here with a legible message.
    let edit_only = window(ctx, id, EventCursor::After(-1), 1000, &[], &["Edit"]).await;
    let kept: Vec<(&str, Option<&str>)> = edit_only
        .iter()
        .map(|e| (e.kind.as_str(), e.tool_name()))
        .collect();
    assert!(
        kept.contains(&("tool_call_requested", Some("Edit"))),
        "requested reads `name`: {kept:?}"
    );
    assert!(
        kept.contains(&("tool_call_started", Some("Edit")))
            && kept.contains(&("tool_call_completed", Some("Edit"))),
        "started/completed read `tool_name`: {kept:?}"
    );
    assert_eq!(
        kept.iter()
            .filter(|(k, _)| *k == "tool_result_submitted")
            .count(),
        2,
        "both results pass through: they carry no tool name"
    );
    assert_eq!(
        kept.iter()
            .filter(|(k, _)| *k == "tool_call_started")
            .count(),
        2,
        "the malformed nameless `tool_call_started` passes through too: {kept:?}"
    );
    assert!(
        kept.iter().any(|(k, _)| *k == "agent_message")
            && kept.iter().any(|(k, _)| *k == "status_changed"),
        "non-tool kinds are unaffected by tool_names: {kept:?}"
    );
    assert!(
        !kept.contains(&("tool_call_started", Some("Bash"))),
        "the other tool's named events are gone: {kept:?}"
    );

    // Both filters intersect: the kind narrows the rows, the tool name
    // narrows the NAMED ones inside that kind. The nameless
    // `tool_call_started` survives here too — the "no name, no filter" rule
    // is a property of the ROW, not of how the caller reached it.
    let started_edit = window(
        ctx,
        id,
        EventCursor::After(-1),
        1000,
        &["tool_call_started"],
        &["Edit"],
    )
    .await;
    assert!(
        started_edit.iter().all(|e| e.kind == "tool_call_started"),
        "the kind filter still applies with a tool filter"
    );
    assert_eq!(
        started_edit
            .iter()
            .map(|e| e.tool_name())
            .collect::<Vec<_>>(),
        vec![Some("Edit"), None],
        "the Edit start, plus the nameless start that no tool filter removes"
    );
}

/// (d) The backward anchor at the two extremes: `Before(0)` is below the
/// first idx, so the page is EMPTY (never "the whole log"), and an anchor
/// past the tail returns the last page, the same one `Before(i64::MAX)`
/// gives. These are the two anchors a transcript hits first — one at the
/// top of the scrollback, one on the very first open.
async fn events_before_bounds_are_empty_and_tail(ctx: &Ctx) {
    let id = ctx
        .meta
        .create_session(spec("conf:window-bounds"))
        .await
        .unwrap();
    const TOTAL: i64 = 12;
    for n in 0..TOTAL {
        ctx.meta
            .append_session_event(id, "agent_message", serde_json::json!({ "n": n }))
            .await
            .unwrap();
    }

    assert!(
        window(ctx, id, EventCursor::Before(0), 100, &[], &[])
            .await
            .is_empty(),
        "Before(0) is strictly below the first event: an empty page"
    );
    // Negative anchors are equally empty, not an error.
    assert!(
        window(ctx, id, EventCursor::Before(-5), 100, &[], &[])
            .await
            .is_empty(),
        "a negative anchor is empty, not a wrap-around"
    );

    let past_tail = window(ctx, id, EventCursor::Before(TOTAL + 100), 5, &[], &[]).await;
    let newest = window(ctx, id, EventCursor::Before(i64::MAX), 5, &[], &[]).await;
    assert_eq!(
        idxs(&past_tail),
        (TOTAL - 5..TOTAL).collect::<Vec<_>>(),
        "an anchor past the tail yields the newest page, re-ascended"
    );
    assert_eq!(idxs(&newest), idxs(&past_tail), "past-the-tail == i64::MAX");

    // And it agrees with the tail wrapper, which is the same window.
    let tail = ctx.meta.list_session_events_tail(id, 5).await.unwrap();
    assert_eq!(
        idxs(&tail),
        idxs(&newest),
        "the tail wrapper is this window"
    );

    // A session with no events at all is empty in both directions.
    let empty = ctx
        .meta
        .create_session(spec("conf:window-empty"))
        .await
        .unwrap();
    assert!(
        window(ctx, empty, EventCursor::Before(i64::MAX), 10, &[], &[])
            .await
            .is_empty()
    );
    assert!(window(ctx, empty, EventCursor::After(-1), 10, &[], &[])
        .await
        .is_empty());
}

/// (e) The limit means the same thing in both directions: a positive limit
/// takes from the anchor's end, and a non-positive one is an EMPTY page,
/// never "unlimited". The second half matters most — a caller that clamps
/// badly must get nothing, not a whole-session read.
async fn events_limit_clamps_identically_both_directions(ctx: &Ctx) {
    let id = ctx
        .meta
        .create_session(spec("conf:window-limit"))
        .await
        .unwrap();
    const TOTAL: i64 = 8;
    for n in 0..TOTAL {
        ctx.meta
            .append_session_event(id, "agent_message", serde_json::json!({ "n": n }))
            .await
            .unwrap();
    }

    for limit in [0i64, -1, -1000] {
        assert!(
            window(ctx, id, EventCursor::After(-1), limit, &[], &[])
                .await
                .is_empty(),
            "forward limit {limit} is an empty page"
        );
        assert!(
            window(ctx, id, EventCursor::Before(i64::MAX), limit, &[], &[])
                .await
                .is_empty(),
            "backward limit {limit} is an empty page"
        );
    }

    // A limit takes from the anchor's end: the OLDEST n forward, the
    // NEWEST n backward.
    for limit in [1i64, 3, TOTAL - 1] {
        let fwd = window(ctx, id, EventCursor::After(-1), limit, &[], &[]).await;
        assert_eq!(idxs(&fwd), (0..limit).collect::<Vec<_>>());
        let back = window(ctx, id, EventCursor::Before(i64::MAX), limit, &[], &[]).await;
        assert_eq!(idxs(&back), (TOTAL - limit..TOTAL).collect::<Vec<_>>());
    }

    // A limit larger than the log returns the whole log, both ways.
    let fwd = window(ctx, id, EventCursor::After(-1), TOTAL * 10, &[], &[]).await;
    let back = window(ctx, id, EventCursor::Before(i64::MAX), TOTAL * 10, &[], &[]).await;
    assert_eq!(idxs(&fwd), (0..TOTAL).collect::<Vec<_>>());
    assert_eq!(idxs(&back), (0..TOTAL).collect::<Vec<_>>());
}

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
    meta.append_session_event(
        id,
        "agent_message",
        serde_json::json!({"role": "assistant", "text": "hi"}),
    )
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
    // ADR 0107: a mode directive is user intent — it survives a rewind.
    meta.append_session_event(
        id,
        "harness_mode_changed",
        serde_json::json!({"mode": "plan"}),
    )
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

    // Only the guest-history events roll back (the assistant
    // agent_message, integration_asset, tool_call_started = 3). The
    // coordinator facts (prompt_received, resume_started, harness_idle,
    // harness_mode_changed, durability_rollback), the anchor
    // status_changed, AND user input (file_shared) survive — the
    // positive-provenance predicate defaults to keep.
    assert_eq!(summary.rolled_back, 3, "rolled_back count");
    assert_eq!(summary.through_idx, cursor, "through_idx is the cursor");
    assert_eq!(summary.recovery_epoch, 1, "epoch bumped once");
    assert_eq!(
        summary.surviving_side_effects,
        vec!["A forge pull_request was produced and still exists: PR #5".to_string()],
        "surviving side-effects: only the tombstoned integration asset — \
         file_shared survives the rewind, so its event stays visible and \
         needs no note",
    );

    // Per-event tombstone state must agree across stores: only
    // guest-history kinds are tombstoned; everything else stays live.
    let events = meta
        .list_session_events_since(id, cursor, 1000)
        .await
        .unwrap();
    for e in &events {
        let guest_history = matches!(
            e.kind.as_str(),
            "agent_message" | "integration_asset" | "tool_call_started"
        );
        assert_eq!(
            e.rewound_at.is_some(),
            guest_history,
            "kind {} rewound_at (guest_history={guest_history})",
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

/// The two session clocks are independent, and both stores must move them
/// on exactly the same triggers.
///
/// `last_active_at` is the STATE-MACHINE clock: stamped at create and by
/// every `transition_session`. `last_event_at` is the ACTIVITY clock
/// (migration 0068): `None` until the first event, then bumped by every
/// `append_session_event`. Crossing them is a live bug in both directions —
/// the eviction scanner keys its idempotency dedup on `last_active_at`
/// staying frozen across an event append, and the orchestrator's task list
/// orders on `last_event_at` NOT moving when a session merely changes state.
async fn session_activity_clock_is_independent(ctx: &Ctx) {
    let meta = &ctx.meta;
    let created_at = ctx.clock.now_utc();
    let id = meta.create_session(spec("conf:activity")).await.unwrap();

    // A session that has not emitted an event has no activity clock — the
    // orchestrator's fallback to `last_active_at` depends on the NULL.
    let s = meta.get_session(id).await.unwrap();
    assert_eq!(s.last_active_at, created_at);
    assert_eq!(s.last_event_at, None, "no events yet ⇒ no activity clock");

    // An event append bumps ONLY the activity clock.
    ctx.clock.advance(Duration::from_secs(60));
    let first_event_at = ctx.clock.now_utc();
    meta.append_session_event(id, "agent_message", serde_json::json!({"text": "hi"}))
        .await
        .unwrap();
    let s = meta.get_session(id).await.unwrap();
    assert_eq!(s.last_event_at, Some(first_event_at));
    assert_eq!(
        s.last_active_at, created_at,
        "an event append must NOT move the state-machine clock"
    );

    // A state transition bumps ONLY the state-machine clock. This is the
    // ordering bug in the task list: the session did nothing new, yet
    // `last_active_at` jumps ahead of its last real event.
    ctx.clock.advance(Duration::from_secs(60));
    let transition_at = ctx.clock.now_utc();
    meta.transition_session(id, SessionState::Failed, BindingDisposition::Detach)
        .await
        .unwrap();
    let s = meta.get_session(id).await.unwrap();
    assert_eq!(s.last_active_at, transition_at);
    assert_eq!(
        s.last_event_at,
        Some(first_event_at),
        "a state transition must NOT move the activity clock"
    );
}

/// `list_active_sessions` feeds the app-facing session list, so it must
/// project the activity clock — an unprojected column silently degrades the
/// task ordering back to `last_active_at` instead of failing loudly.
async fn list_active_sessions_projects_activity_clock(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta
        .create_session(spec("conf:activity-list"))
        .await
        .unwrap();
    ctx.clock.advance(Duration::from_secs(30));
    let event_at = ctx.clock.now_utc();
    meta.append_session_event(id, "agent_message", serde_json::json!({"text": "hi"}))
        .await
        .unwrap();

    let listed = meta.list_active_sessions().await.unwrap();
    let s = listed
        .iter()
        .find(|s| s.id == id)
        .expect("pending session is in the active set");
    assert_eq!(s.last_event_at, Some(event_at));
}

/// ADR 0026 artifacts: insert/get roundtrip (incl. `file_name`, added by
/// migration 0110), session scoping, and usage aggregation.
async fn artifact_insert_get_usage(ctx: &Ctx) {
    let sid = ctx
        .meta
        .create_session(spec("test.invalid/artifacts:latest"))
        .await
        .unwrap();
    let id = uuid::Uuid::from_u128(0xA1);
    ctx.meta
        .insert_artifact(
            id,
            sid,
            "artifacts/k1",
            "text/html",
            42,
            Some("cap"),
            Some("report.html"),
        )
        .await
        .unwrap();

    let row = ctx.meta.get_artifact(sid, id).await.unwrap().unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.blob_key, "artifacts/k1");
    assert_eq!(row.media_type, "text/html");
    assert_eq!(row.size_bytes, 42);
    assert_eq!(row.caption.as_deref(), Some("cap"));
    assert_eq!(row.file_name.as_deref(), Some("report.html"));

    // NULL caption + file_name round-trip as None.
    let id2 = uuid::Uuid::from_u128(0xA2);
    ctx.meta
        .insert_artifact(id2, sid, "artifacts/k2", "image/png", 8, None, None)
        .await
        .unwrap();
    let row2 = ctx.meta.get_artifact(sid, id2).await.unwrap().unwrap();
    assert_eq!(row2.caption, None);
    assert_eq!(row2.file_name, None);

    // Session scoping: a valid id under the wrong session is None.
    let other = ctx
        .meta
        .create_session(spec("test.invalid/artifacts-b:latest"))
        .await
        .unwrap();
    assert!(ctx.meta.get_artifact(other, id).await.unwrap().is_none());

    // A duplicate id is an error on both stores (PK).
    let dup = ctx
        .meta
        .insert_artifact(id, sid, "artifacts/k1", "text/html", 42, None, None)
        .await;
    assert!(dup.is_err(), "duplicate artifact id must error");

    // Usage aggregates count + bytes for the session only.
    assert_eq!(ctx.meta.artifact_usage(sid).await.unwrap(), (2, 50));
    assert_eq!(ctx.meta.artifact_usage(other).await.unwrap(), (0, 0));
}

/// Migration 0110 semantics: artifact rows have no FK to `sessions` —
/// an insert for a session id with no sessions row succeeds and reads
/// back (rows outlive their session; the cross-session registry
/// references them by id).
async fn artifact_outlives_sessions(ctx: &Ctx) {
    let ghost = SessionId::from(uuid::Uuid::from_u128(0xDEAD));
    let id = uuid::Uuid::from_u128(0xA3);
    ctx.meta
        .insert_artifact(
            id,
            ghost,
            "artifacts/ghost",
            "text/markdown",
            7,
            None,
            Some("notes.md"),
        )
        .await
        .unwrap();
    let row = ctx.meta.get_artifact(ghost, id).await.unwrap().unwrap();
    assert_eq!(row.media_type, "text/markdown");
    assert_eq!(row.file_name.as_deref(), Some("notes.md"));
    assert_eq!(ctx.meta.artifact_usage(ghost).await.unwrap(), (1, 7));
}

/// Prod 2026-08-03 (session aa0829b0, 12 sessions/14d): a prompt sent
/// to an idle session was appended before the un-park resume ran its
/// rung-1 rewind, and the rewind tombstoned the user's own message —
/// surfacing as a spurious `recovered_from_checkpoint{checkpoint_lag,
/// rolled_back: 1}`. The positive-provenance predicate makes user
/// input un-tombstoneable by construction: the race is benign at every
/// interleaving, and a clean idle resume (nothing but input past the
/// cursor) rolls back ZERO rows — so `apply_rung1_rewind` emits no
/// recovery event at all. Genuine lag (guest rows past the cursor)
/// must still count and still fire — non-vacuity is asserted here too.
async fn rewind_never_tombstones_user_input(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta
        .create_session(spec("conf:rewind-input"))
        .await
        .unwrap();

    let cursor = meta
        .append_session_event(id, "status_changed", serde_json::json!({"to": "idle"}))
        .await
        .unwrap();

    // The incident shape: ONLY user input lands past the cursor (the
    // prompt echo, a deferred tool answer, an uploaded file) plus an
    // unknown future kind — nothing the guest produced.
    meta.append_session_event(
        id,
        "agent_message",
        serde_json::json!({"role": "user", "text": "ok resume"}),
    )
    .await
    .unwrap();
    meta.append_session_event(
        id,
        "tool_result_submitted",
        serde_json::json!({"tool_call_id": "t1", "result_json": "{}"}),
    )
    .await
    .unwrap();
    meta.append_session_event(
        id,
        "file_shared",
        serde_json::json!({"caption": "spec.pdf", "artifact_id": "art9"}),
    )
    .await
    .unwrap();
    meta.append_session_event(id, "some_future_kind", serde_json::json!({}))
        .await
        .unwrap();

    // A clean idle resume: zero rows tombstoned, zero epoch bump, and
    // therefore (in the coordinator) zero recovery events emitted.
    let summary = meta.rewind_session_to_cursor(id, cursor).await.unwrap();
    assert_eq!(summary.rolled_back, 0, "user input never rolls back");
    assert_eq!(summary.recovery_epoch, 0, "no-op rewind bumps nothing");
    let events = meta
        .list_session_events_since(id, cursor, 1000)
        .await
        .unwrap();
    assert!(
        events.iter().all(|e| e.rewound_at.is_none()),
        "no row past the cursor was tombstoned",
    );

    // Non-vacuity: with GENUINE guest history past the cursor, the
    // rewind still counts exactly the guest rows — user input still
    // survives alongside them.
    meta.append_session_event(id, "run_started", serde_json::json!({"run_id": "r1"}))
        .await
        .unwrap();
    meta.append_session_event(
        id,
        "agent_message",
        serde_json::json!({"role": "assistant", "text": "working…"}),
    )
    .await
    .unwrap();
    let summary = meta.rewind_session_to_cursor(id, cursor).await.unwrap();
    assert_eq!(
        summary.rolled_back, 2,
        "genuine guest history still counts (run_started + assistant message)",
    );
    assert_eq!(summary.recovery_epoch, 1, "genuine lag bumps the epoch");
    let events = meta
        .list_session_events_since(id, cursor, 1000)
        .await
        .unwrap();
    for e in &events {
        let guest_history = matches!(e.kind.as_str(), "run_started")
            || (e.kind == "agent_message"
                && e.payload.get("role").and_then(|v| v.as_str()) == Some("assistant"));
        assert_eq!(
            e.rewound_at.is_some(),
            guest_history,
            "kind {} (payload {:?}) tombstone state",
            e.kind,
            e.payload,
        );
    }
}

conformance!(
    t_rewind_excludes_coordinator_facts,
    super::rewind_excludes_coordinator_facts
);
conformance!(
    t_rewind_never_tombstones_user_input,
    super::rewind_never_tombstones_user_input
);

/// The idempotent prompt accept (PR #556 finding #3): a retry with the
/// same `prompt_id` appends NOTHING — no duplicate receipt, no
/// duplicate user echo — and a `prompt_id` claimed by a different
/// command is a Conflict.
async fn prompt_accept_is_idempotent_on_prompt_id(ctx: &Ctx) {
    let meta = &ctx.meta;
    let id = meta.create_session(spec("conf:accept")).await.unwrap();
    let now = ctx.clock.now_utc();

    let row = engram_core::types::outbox::OutboxRow {
        prompt_id: "p-accept-1".into(),
        session_id: id,
        kind: engram_core::types::outbox::OutboxKind::Prompt,
        payload: serde_json::json!({"text": "hello"}),
        created_at: now,
        attempts: 0,
        not_before: now,
        delivered_at: None,
        acked_at: None,
    };
    let events = vec![
        (
            "prompt_received".to_string(),
            serde_json::json!({"prompt_id": "p-accept-1"}),
        ),
        (
            "agent_message".to_string(),
            serde_json::json!({"role": "user", "text": "hello", "prompt_id": "p-accept-1"}),
        ),
    ];

    // Fresh accept: both events append, in order, and the row exists.
    let idxs = meta
        .append_events_with_outbox_idempotent(id, &events, &row)
        .await
        .unwrap()
        .expect("fresh accept appends");
    assert_eq!(idxs.len(), 2, "one idx per event");
    assert!(idxs[0] < idxs[1], "events append in order");
    let after_first = meta.list_session_events_since(id, -1, 1000).await.unwrap();

    // Retry (same prompt_id, same command): appends NOTHING.
    let retry = meta
        .append_events_with_outbox_idempotent(id, &events, &row)
        .await
        .unwrap();
    assert!(retry.is_none(), "retry is the designed no-op");
    let after_retry = meta.list_session_events_since(id, -1, 1000).await.unwrap();
    assert_eq!(
        after_first.len(),
        after_retry.len(),
        "a retry appends no duplicate receipt/echo rows",
    );

    // Same prompt_id, DIFFERENT command: corruption, not idempotency.
    let mut stolen = row.clone();
    stolen.kind = engram_core::types::outbox::OutboxKind::ToolResult;
    let err = meta
        .append_events_with_outbox_idempotent(id, &events, &stolen)
        .await
        .expect_err("a different command on the same prompt_id must conflict");
    assert!(
        matches!(err, engram_core::error::MetaError::Conflict(_)),
        "got {err:?}",
    );

    // Same prompt_id, same kind, DIFFERENT payload (text/mode): the
    // payload is part of the command identity — a divergent re-send
    // conflicts loudly instead of silently dropping the new text
    // (review finding on #993).
    let mut divergent = row.clone();
    divergent.payload = serde_json::json!({"text": "something else entirely"});
    let err = meta
        .append_events_with_outbox_idempotent(id, &events, &divergent)
        .await
        .expect_err("a divergent payload on the same prompt_id must conflict");
    assert!(
        matches!(err, engram_core::error::MetaError::Conflict(_)),
        "got {err:?}",
    );
    let after_divergent = meta.list_session_events_since(id, -1, 1000).await.unwrap();
    assert_eq!(
        after_first.len(),
        after_divergent.len(),
        "a rejected divergent re-send appends nothing",
    );
}

conformance!(
    t_prompt_accept_is_idempotent_on_prompt_id,
    super::prompt_accept_is_idempotent_on_prompt_id
);
conformance!(
    t_list_session_events_pages,
    super::list_session_events_pages_without_gap_or_dup
);
conformance!(
    t_events_backward_walk_equals_forward_walk,
    super::events_backward_walk_equals_forward_walk
);
conformance!(
    t_events_kind_filter,
    super::events_kind_filter_equals_in_memory_filter
);
conformance!(
    t_events_tool_name_filter,
    super::events_tool_name_filter_spares_nameless_kinds
);
conformance!(
    t_events_before_bounds,
    super::events_before_bounds_are_empty_and_tail
);
conformance!(
    t_events_limit_clamps,
    super::events_limit_clamps_identically_both_directions
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
conformance!(
    t_session_activity_clock,
    super::session_activity_clock_is_independent
);
conformance!(
    t_list_active_sessions_activity_clock,
    super::list_active_sessions_projects_activity_clock
);
conformance!(
    t_artifact_insert_get_usage,
    super::artifact_insert_get_usage
);
conformance!(
    t_artifact_outlives_sessions,
    super::artifact_outlives_sessions
);
