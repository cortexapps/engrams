//! Live-Postgres tests for the ADR 0048 session-queue store layer:
//! `reserve_and_persist_create`'s Queued disposition (issue #535 (b);
//! formerly `enqueue_session_create`), `list_queued_sessions_fifo`,
//! `place_queued_session` (the queued-row reservation transaction), and
//! `queued_demand` — all against REAL Postgres (the migration 0064
//! schema + the FIFO index + the queued→pending flip). ADR 0079: the
//! scanner's boot half is the create_boot OP now — placement enqueues a
//! `session_ops` row (asserted below) and the op executor owns boot
//! retry + crash recovery (`requeue_session`/`requeue_stale_pending`
//! are retired); the boot pipeline itself is covered by the api.rs
//! create tests + e2e_stack.
//!
//! Also covers the queue-fairness follow-up (per-fit-class scanner
//! sweeps + the `placement_changed` NOTIFY wake): `queue_scanner::run_once`
//! is `pub` specifically so these tests can drive it deterministically
//! against real placement SQL (the mock `MetadataStore` used by `api.rs`
//! can't exercise the 2D fit — its `place_queued_session` default just
//! places on the first candidate).
//!
//! `#[ignore]`'d by default; requires Postgres at `ENGRAM_TEST_DATABASE_URL`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_coordinator::queue_scanner::{self, QueueScannerConfig};
use engram_core::traits::MetadataStore;
use engram_core::types::host::{
    HostCapacity, HostHeartbeat, HostRecord, HostStatus, HostUtilization,
};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::session::{QueueOrigin, SessionMode, SessionSpec, SessionState};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{EnabledImage, HostId, SessionId};
use uuid::Uuid;

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
    }
}

/// Issue #535 (b): `enqueue_session_create` retired — `reserve_and_persist_
/// create` with an EMPTY candidate list is its structural replacement (no
/// host can ever fit zero candidates, so the disposition is always
/// `Queued`). Thin wrapper so the seeding call sites below read the same as
/// before the refactor.
async fn enqueue(
    meta: &Arc<dyn MetadataStore>,
    session_id: SessionId,
    spec: SessionSpec,
    mem_budget_mib: i64,
    cpu_budget_vcpus: i32,
) {
    let ws = engram_core::traits::SessionCreateWriteSet {
        session_id,
        spec,
        mem_budget_mib,
        cpu_budget_vcpus,
        sealed_secrets: None,
        capabilities: Vec::new(),
        integration_policy_json: None,
        runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(Vec::new(), None, None),
    };
    let disposition = meta
        .reserve_and_persist_create(ws, &[], 0)
        .await
        .expect("reserve_and_persist_create (enqueue)");
    assert_eq!(
        disposition,
        engram_core::traits::CreateDisposition::Queued,
        "empty candidates must always disposition Queued"
    );
}

/// Issue #535 (b): `reserve_placement` retired — `reserve_and_persist_create`
/// is its structural replacement. Thin wrapper collapsing `CreateDisposition`
/// back to `Option<HostId>` so this file's direct (non-`place_create`)
/// reservation call sites read the same as before the refactor (mirrors
/// `placement_reservation_live_pg.rs`'s identical shim).
async fn reserve(
    meta: &Arc<dyn MetadataStore>,
    session_id: SessionId,
    spec: &SessionSpec,
    mem_budget_mib: i64,
    cpu_budget_vcpus: i32,
    candidates: &[HostId],
    affinity_len: usize,
) -> Option<HostId> {
    let ws = engram_core::traits::SessionCreateWriteSet {
        session_id,
        spec: spec.clone(),
        mem_budget_mib,
        cpu_budget_vcpus,
        sealed_secrets: None,
        capabilities: Vec::new(),
        integration_policy_json: None,
        runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(Vec::new(), None, None),
    };
    match meta
        .reserve_and_persist_create(ws, candidates, affinity_len)
        .await
        .expect("reserve_and_persist_create ok")
    {
        engram_core::traits::CreateDisposition::Placed(h) => Some(h),
        engram_core::traits::CreateDisposition::Queued => None,
    }
}

/// `ready_images` is the set of manifest digests this host's heartbeat
/// advertises as staged — PR #565's `place_create` digest gate
/// (`queue_scanner.rs`) only offers a host as a candidate for a queued
/// create if its `ready_images` contains that create's enabled image's
/// digest (`placement::host_passes_filters`). Empty for the tests that
/// don't drive a create through the gate (they call `place_queued_session`
/// / `reserve_and_persist_create` directly against the store, bypassing
/// `place_create`).
async fn seed_ready_host(
    meta: &Arc<dyn MetadataStore>,
    allocatable_mib: u64,
    total_vcpus: u32,
    ready_images: &[String],
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
            ready_images: ready_images.to_vec(),
            current_bundles: Vec::new(),
            total_vcpus,
            // Issue #229: report the coordinator's wire version so the
            // placement filter keeps this seeded host schedulable.
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
        },
    )
    .await
    .expect("heartbeat host");
    id
}

/// Seeds a live `enabled_images` row for `image_uri` with a freshly
/// generated, unique digest and returns that digest.
///
/// PR #565's `place_create` (`queue_scanner.rs`) does a live-only
/// `get_enabled_image` lookup before placing a queued create; `Ok(None)`
/// (no row / soft-deleted) now terminally fails the session
/// (`fail_queued_create_image_gone`) instead of leaving it queued. Every
/// session this file enqueues through the scanner (`place_create`, i.e.
/// anything driven via `queue_scanner::run_once`) therefore needs its own
/// enabled-image row, even sessions engineered to be capacity-unfittable —
/// otherwise they'd be observed terminally Failed (image-shaped) instead of
/// legitimately Queued forever (capacity-shaped), breaking this file's
/// per-fit-class assertions. `base_snapshot_id` is `NOT NULL REFERENCES
/// snapshots(id)` (migration 0038), so a real (if content-empty) snapshot
/// row is recorded first to satisfy the FK — its content is never actually
/// materialized in these tests (the scanner's own boot attempt fails fast
/// against the local, unpopulated blob store and requeues, same as this
/// file's pre-existing "no enabled_images row" comments described before
/// PR #565 added the gate).
async fn seed_enabled_image(meta: &Arc<dyn MetadataStore>, image_uri: &str) -> String {
    let snapshot_id = engram_core::SnapshotId::new();
    meta.record_snapshot(SnapshotRecord {
        id: snapshot_id,
        session_id: None,
        host_id: None,
        image_version: image_uri.to_string(),
        size_bytes: 0,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: None,
        memory_manifest: None,
        recoverable: true,
        aux_bundles: vec![],
        events_cursor: None,
        fc_snapshot_version: None,
    })
    .await
    .expect("seed base snapshot for enabled image");

    let digest = format!("sha256:{}", Uuid::new_v4().simple());
    let now = Utc::now();
    meta.upsert_enabled_image(EnabledImage {
        id: Uuid::new_v4(),
        image_uri: image_uri.to_string(),
        manifest_toml: format!("name = \"queue-scanner-fixture-{}\"\n", Uuid::new_v4()),
        manifest_digest: digest.clone(),
        disk_manifest: None,
        base_snapshot_id: Some(snapshot_id),
        base_snapshot_disk_manifest: Some(ManifestRef::new()),
        base_snapshot_memory_manifest: None,
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
        soft_deleted_at: None,
        capture_env: Vec::new(),
    })
    .await
    .expect("seed enabled image");
    digest
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn enqueue_list_demand_and_fifo_order() {
    let Some(meta) = connect().await else { return };

    let s1 = SessionId::new();
    let s2 = SessionId::new();
    // s1 enqueued first → must sort first (FIFO by queued_at).
    enqueue(&meta, s1, spec(), 4096, 2).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    enqueue(&meta, s2, spec(), 8192, 4).await;

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
    assert_eq!(q1.mem_budget_mib, 4096);
    assert_eq!(q1.cpu_budget_vcpus, 2);
    assert_eq!(q1.session.status, SessionState::Queued);
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn place_queued_flips_to_pending_on_a_fitting_host() {
    let Some(meta) = connect().await else { return };
    let host = seed_ready_host(&meta, 16_384, 8, &[]).await;
    let sid = SessionId::new();
    enqueue(&meta, sid, spec(), 4096, 2).await;

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
    let host = seed_ready_host(&meta, 2048, 8, &[]).await;
    let sid = SessionId::new();
    enqueue(&meta, sid, spec(), 4096, 2).await;
    let placed = meta
        .place_queued_session(sid, 4096, 2, &[host], 0)
        .await
        .expect("place");
    assert_eq!(placed, None, "no host fits → stay queued");
    let row = meta.get_session(sid).await.expect("get");
    assert_eq!(row.status, SessionState::Queued, "row stays queued");
}

/// ADR 0079: the successor to the retired `requeue_and_stale_pending_
/// recovery` test. Placement no longer bounces a failed boot back to the
/// queue by poll — the sweep's `queued → pending` flip enqueues a
/// `create_boot` op (keyed `boot:{session}:{queued_at millis}`) whose
/// row-level retry/reclaim own everything the requeue paths used to.
/// Asserts: the sweep placed the session (durable Queued→Pending event),
/// a create_boot op is pending for it, and a sibling replica's re-derived
/// key dedups instead of double-enqueueing.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn placement_enqueues_create_boot_op_with_stable_key() {
    use engram_core::types::session_op::{EnqueueOutcome, OpKind};
    let Some((meta, state, _database_url)) = setup(true).await else {
        return;
    };

    // Same fixture shape as `hol_break_is_per_class_not_global`: a host
    // sized EXACTLY to this session's own randomized budget (see that
    // test's comment for why), the enabled-image row PR #565's digest
    // gate requires, and the digest staged on the host.
    let sid = SessionId::new();
    let (mem, cpu) = unique_fitting_budget_in(sid, BOOT_OP_BUDGET_BASE_MIB);
    let s_spec = spec();
    let digest = seed_enabled_image(&meta, &s_spec.image).await;
    let _host = seed_ready_host(&meta, mem as u64, 8, &[digest]).await;
    enqueue(&meta, sid, s_spec, mem, cpu).await;
    let queued_at = meta
        .list_queued_sessions_fifo()
        .await
        .expect("list queued")
        .into_iter()
        .find(|q| q.session.id == sid)
        .expect("our queued row")
        .queued_at;

    queue_scanner::run_once(&QueueScannerConfig::default(), &state)
        .await
        .expect("run_once");

    assert!(
        has_status_change_to(&meta, sid, "pending").await,
        "the sweep must have placed the session (durable Queued→Pending event)"
    );
    assert!(
        meta.op_pending_exists(sid, OpKind::CreateBoot)
            .await
            .expect("op_pending_exists"),
        "placement must enqueue a create_boot op (the boot's durable owner)"
    );
    // The key is derived from durable row state (`queued_at`), so a
    // sibling replica racing the same placement re-derives the SAME key
    // and dedups instead of appending a second boot op.
    let key = format!("boot:{sid}:{}", queued_at.timestamp_millis());
    let dup = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::CreateBoot,
            serde_json::json!({}),
            Some(&key),
            "rival-pod",
        )
        .await
        .expect("duplicate enqueue");
    assert!(
        matches!(dup, EnqueueOutcome::Duplicate),
        "the stable idempotency key must dedup a sibling's re-enqueue, got {dup:?}"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn resume_origin_enqueue_requires_idle() {
    let Some(meta) = connect().await else { return };
    // A non-idle session is a no-op for the resume enqueue (gated on
    // status='idle'); we just assert it doesn't error and doesn't queue.
    let sid = SessionId::new();
    enqueue(&meta, sid, spec(), 4096, 2).await;
    // It's `queued`, not `idle`, so enqueue_session_resume is a no-op.
    meta.enqueue_session_resume(sid)
        .await
        .expect("resume enqueue no-op");
    let row = meta.get_session(sid).await.unwrap();
    // Still a create-origin queued row.
    assert_eq!(row.status, SessionState::Queued);
}

// ─── queue-fairness follow-up: per-fit-class scanner sweeps + the
// ─── `placement_changed` NOTIFY wake ───────────────────────────────────
//
// These tests share ONE live Postgres with every other live-pg test in
// this crate, and `run_once`'s placement path reads the FULL `hosts` /
// `sessions` tables (`candidates_for` → `list_active_hosts`, and the
// queue sweep itself, are both unscoped by test). nextest's DEFAULT is to
// run each `#[ignore]`'d test as its own process, in parallel — but this
// file is NOT safe under that default: `ci.yml`'s "Postgres-gated ignored
// tests" step runs the whole live-pg group with `--test-threads=1`, and
// this file's tests actually require that serialization to be correct,
// not just fast. In particular, `create_origin_timeout_fails_session_and_records_wait`
// / `resume_origin_timeout_returns_to_idle` run `run_once` with a 1ms
// timeout, which times out (Failed / Idle) EVERY queued row in the shared
// database, not just their own — running them concurrently with any other
// live-pg test that has an in-flight `queued` row would brick it. Sessions
// that must NOT fit anywhere use a budget (1 TiB / 1000 vcpus) no other
// test in this suite could accidentally satisfy; sessions that DO need
// to fit are only asserted by "left `queued`", never by which host they
// landed on, so accidentally fitting a concurrently-seeded foreign host
// is harmless — but the destructive timeout sweeps above are not, and
// depend on `--test-threads=1` for correctness, not merely determinism.

/// A budget no live-pg test in this suite seeds a host large enough to
/// satisfy — the "doesn't fit anywhere, ever" budget. Must stay LARGER
/// than every other host capacity this file ever seeds (including
/// [`SCANNER_WAKE_MEM_MIB`] below) — the whole point is that nothing,
/// including a leftover host from another test's teardown-less run,
/// could ever accidentally satisfy it.
const UNFITTABLE_MEM_MIB: i64 = 50_000_000;
const UNFITTABLE_CPU_VCPUS: i32 = 50_000;

/// A distinctive magnitude reserved for
/// `scanner_wakes_on_notify_and_places_within_the_wake_not_the_fallback`'s
/// dedicated host: that test's host starts out genuinely full (not
/// unfittable-by-design like [`UNFITTABLE_MEM_MIB`]) and ends the test
/// with its reservation released, so no other live-pg test in this file
/// seeds a host anywhere near this size (avoiding accidental fit), and it
/// stays comfortably BELOW `UNFITTABLE_MEM_MIB` (so a freed leftover host
/// from a past run of this test can never satisfy an `UNFITTABLE_MEM_MIB`
/// session elsewhere in the file).
const SCANNER_WAKE_MEM_MIB: i64 = 5_000_000;

async fn build_app_state(
    meta: Arc<dyn MetadataStore>,
    database_url: &str,
) -> Arc<engram_coordinator::AppState> {
    use engram_cloud_mock::MockCloud;
    use engram_coordinator::{AppState, CoordinatorConfig, HostRegistry, Services};

    let work_dir = tempfile::tempdir().expect("work dir").keep();
    let raw: Arc<dyn engram_core::traits::SandboxBackend> =
        Arc::new(engram_sandbox_process::ProcessBackend::new(work_dir));
    let pooled: Arc<dyn engram_core::traits::SandboxBackend> =
        Arc::new(engram_host_agent::pooled_backend::PooledBackend::new(raw));

    let services = Services {
        meta: meta.clone(),
        cloud: Arc::new(MockCloud::new()),
        host: Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(pooled)),
        secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0u8; 32], "test:v1",
        )),
        oci: Arc::new(engram_oci::OciClient::new(Arc::new(
            engram_oci::AnonymousResolver,
        ))),
        auth_resolver: Arc::new(engram_oci::AnonymousResolver),
        blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
            std::env::temp_dir().join("engram-blobs-test-queue-scanner"),
        )),
        chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
            engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test-queue-scanner"),
            ),
        )),
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        materialize_dir: None,
    };
    let cfg = CoordinatorConfig {
        database_url: database_url.to_string(),
        ..CoordinatorConfig::default()
    };
    let registry = Arc::new(HostRegistry::new(meta.clone()));
    Arc::new(AppState::new_with_registry(cfg, services, registry))
}

/// A `(mem, cpu)` budget derived from `id`, restricted to the disjoint
/// `[base, base+512)` MiB sub-range the CALLER owns — a fit CLASS no other
/// session (this test run, a concurrently-running test on the same shared
/// dev Postgres, or a leftover row from a previous run — this suite
/// doesn't clean up after itself, matching the rest of this file) could
/// plausibly share, so a session enqueued with it never contends for host
/// capacity with an unrelated queue row.
///
/// The per-caller `base` matters, not just the per-`id` randomization
/// within it: `hol_break_is_per_class_not_global`'s `B` is left `queued`
/// forever whenever its `boot_placed_create` requeue races the test's own
/// assertions (this harness has no `enabled_images` row, so the boot
/// always fails and requeues), and this suite never cleans that up. A
/// single shared range meant a leftover `B` from one run could later
/// collide with `per_class_fifo_head_block_is_scoped_to_its_class`'s `y1`
/// class on a rerun against a shared dev Postgres (~1/512 chance per
/// leftover) and steal its exactly-sized host before `y1` got attempted —
/// see finding 1 on PR #559. Disjoint ranges make that collision
/// impossible by construction instead of merely unlikely.
///
/// Comfortably under every host this file seeds (smallest is 2048 MiB).
fn unique_fitting_budget_in(id: SessionId, base: i64) -> (i64, i32) {
    let low = (id.as_uuid().as_u128() & 0xffff) as i64; // 0..=65535
    (base + (low % 512), 1)
}

/// [`unique_fitting_budget_in`]'s range for `hol_break_is_per_class_not_global`.
const HOL_BREAK_BUDGET_BASE_MIB: i64 = 1024; // 1024..=1535 MiB

/// [`unique_fitting_budget_in`]'s range for
/// `per_class_fifo_head_block_is_scoped_to_its_class` — disjoint from
/// [`HOL_BREAK_BUDGET_BASE_MIB`] by more than the 512-wide span either
/// draws from.
const PER_CLASS_FIFO_BUDGET_BASE_MIB: i64 = 2048; // 2048..=2559 MiB

/// [`unique_fitting_budget_in`]'s range for
/// `placement_enqueues_create_boot_op_with_stable_key` — disjoint from
/// both ranges above (ADR 0079).
const BOOT_OP_BUDGET_BASE_MIB: i64 = 3072; // 3072..=3583 MiB

/// Does `session_id`'s durable event log contain a `status_changed` event
/// whose `to` field is `to`? Durable proof that the scanner attempted (and
/// won) a placement for this session THIS sweep — stable even if a
/// downstream boot failure later requeues the row (this test harness has
/// no `enabled_images` row, so `prepare_from_row` always fails fast and
/// the boot task requeues; the DB status alone would race that requeue).
async fn has_status_change_to(
    meta: &Arc<dyn MetadataStore>,
    session_id: SessionId,
    to: &str,
) -> bool {
    let events = meta
        .list_session_events_since(session_id, -1, 100)
        .await
        .expect("list events");
    events.iter().any(|e| {
        e.kind == "status_changed" && e.payload.get("to").and_then(|v| v.as_str()) == Some(to)
    })
}

/// Defensive test hygiene: `choose_placement_host`'s last-resort tier
/// (`engram-postgres/src/lib.rs`) treats a `ready`/`draining`, uncordoned
/// host with `allocatable_mib <= 0` ("unmeasured — don't gate on a bogus
/// 0, let dev/new hosts work") as fitting ANY budget, however large.
/// Several OTHER live-pg tests in this crate (`host_register_live_pg`,
/// `binding_cas_live_pg`, `enable_reuse_live_pg`, `admin_evac_live_pg`)
/// call `upsert_host` without a follow-up `touch_host_heartbeat` and
/// never clean up, leaving exactly such a host behind — a real
/// nondeterminism risk for this file's "never fits anywhere" assertions,
/// since the whole live-pg suite runs against ONE shared Postgres in one
/// `--test-threads=1` nextest invocation (`ci.yml`'s "Postgres-gated
/// ignored tests" step). Cordon any such host up front so a test's
/// deliberately-unfittable session can't land on it via that fallback.
async fn cordon_unmeasured_hosts(meta: &Arc<dyn MetadataStore>) {
    if let Ok(hosts) = meta.list_active_hosts().await {
        for h in hosts {
            if h.utilization.allocatable_mib == 0 && !h.cordoned {
                let _ = meta.set_host_cordoned(h.id, true).await;
            }
        }
    }
}

/// Shared preamble for the queue-fairness tests below: connect, optionally
/// cordon unmeasured hosts (`cordon_unmeasured_hosts` — only the tests
/// asserting something never fits anywhere need this; the timeout and
/// NOTIFY-wake tests don't), and build a full `AppState` wired to this
/// same Postgres so `queue_scanner::run_once` (and, for the wake test,
/// `queue_scanner::spawn`) can be driven directly. Every test below
/// repeated this same connect+cordon+`build_app_state` preamble verbatim;
/// extracted to a single fixture helper instead (test-fixture dedupe, per
/// review on PR #559).
async fn setup(
    cordon: bool,
) -> Option<(
    Arc<dyn MetadataStore>,
    Arc<engram_coordinator::AppState>,
    String,
)> {
    let meta = connect().await?;
    if cordon {
        cordon_unmeasured_hosts(&meta).await;
    }
    let database_url = std::env::var("ENGRAM_TEST_DATABASE_URL").unwrap();
    let state = build_app_state(meta.clone(), &database_url).await;
    Some((meta, state, database_url))
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn hol_break_is_per_class_not_global() {
    let Some((meta, state, _database_url)) = setup(true).await else {
        return;
    };

    // Size the host EXACTLY to B's own randomized budget (not a round
    // shared number like 4096) — otherwise a leftover, never-cleaned-up
    // `queued` row from an UNRELATED test elsewhere in this file (several
    // use a fixed 4096/2 budget) could fit on a round-sized host too, get
    // placed onto it by this same sweep, and consume the capacity B
    // needs before the sweep reaches B's class.
    // PR #565's `place_create` digest-gates every queued create on a LIVE
    // `enabled_images` row for its own image (see `seed_enabled_image`) —
    // A and B each need their own row (distinct `spec()` images), even
    // though A is engineered to be capacity-unfittable: without a row,
    // A would be observed terminally Failed (image-shaped) instead of
    // legitimately Queued forever (capacity-shaped), which is what this
    // test actually asserts below. Only B's digest needs to be staged on
    // the host — A never gets past the capacity check regardless.
    let b = SessionId::new();
    let (b_mem, b_cpu) = unique_fitting_budget_in(b, HOL_BREAK_BUDGET_BASE_MIB);
    let b_spec = spec();
    let b_digest = seed_enabled_image(&meta, &b_spec.image).await;
    let host = seed_ready_host(&meta, b_mem as u64, 8, &[b_digest]).await;

    // A (unfittable, older) then B (fits, newer) — the pre-fix scanner's
    // whole-sweep FIFO break at A would leave B untouched this tick.
    let a = SessionId::new();
    let a_spec = spec();
    seed_enabled_image(&meta, &a_spec.image).await;
    enqueue(&meta, a, a_spec, UNFITTABLE_MEM_MIB, UNFITTABLE_CPU_VCPUS).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    enqueue(&meta, b, b_spec, b_mem, b_cpu).await;

    let summary = queue_scanner::run_once(&QueueScannerConfig::default(), &state)
        .await
        .expect("run_once");
    // A and B are different fit classes (different budgets), so A's
    // NoCapacity must not have stopped the sweep before reaching B.
    assert!(
        summary.placed >= 1 || has_status_change_to(&meta, b, "pending").await,
        "B (a different, fitting class) must be attempted in the SAME sweep A's \
         unfittable head is skipped in — this is the headline HOL fix"
    );
    assert_eq!(
        meta.get_session(a).await.unwrap().status,
        SessionState::Queued,
        "A (unfittable anywhere) must remain queued — it never got a placement"
    );
    assert!(
        !has_status_change_to(&meta, a, "pending").await,
        "A must never have been attempted for placement (it can't fit)"
    );
    assert!(
        has_status_change_to(&meta, b, "pending").await,
        "B must have been placed (queued → pending) in this same sweep, not \
         blocked behind A's class"
    );

    // Autoscaler signal unchanged: the still-queued, unplaceable head A
    // still counts toward demand.
    let demand = meta.queued_demand().await.expect("demand");
    assert!(demand.sessions >= 1);
    assert!(demand.mem_mib >= UNFITTABLE_MEM_MIB as u64);

    let _ = host; // keep the host row alive for the duration of the test
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn per_class_fifo_head_block_is_scoped_to_its_class() {
    let Some((meta, state, _database_url)) = setup(true).await else {
        return;
    };

    // Size the host EXACTLY to Y1's own randomized budget — see the
    // comment in `hol_break_is_per_class_not_global` for why a round
    // shared capacity (e.g. 4096) is vulnerable to an unrelated leftover
    // `queued` row (several other tests in this file use a fixed 4096/2
    // budget and never clean up) stealing the host's capacity mid-sweep.
    // Draws from a disjoint sub-range from `hol_break`'s own `B` (see
    // `unique_fitting_budget_in`) so a leftover `B` row from a previous
    // local run can never collide with this test's class either.
    // See the digest-gate comment in `hol_break_is_per_class_not_global`:
    // every queued create needs its own live `enabled_images` row (PR
    // #565's `place_create` gate), including x1/x2 (which must stay
    // Queued for the RIGHT reason — capacity, not a missing image row).
    // Only y1's digest needs to be staged on the host.
    let y1 = SessionId::new();
    let (y1_mem, y1_cpu) = unique_fitting_budget_in(y1, PER_CLASS_FIFO_BUDGET_BASE_MIB);
    let y1_spec = spec();
    let y1_digest = seed_enabled_image(&meta, &y1_spec.image).await;
    let _host = seed_ready_host(&meta, y1_mem as u64, 8, &[y1_digest]).await;

    // Class X: two SAME-budget (unfittable) sessions, x1 older than x2.
    let x1 = SessionId::new();
    let x1_spec = spec();
    seed_enabled_image(&meta, &x1_spec.image).await;
    enqueue(&meta, x1, x1_spec, UNFITTABLE_MEM_MIB, UNFITTABLE_CPU_VCPUS).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let x2 = SessionId::new();
    let x2_spec = spec();
    seed_enabled_image(&meta, &x2_spec.image).await;
    enqueue(&meta, x2, x2_spec, UNFITTABLE_MEM_MIB, UNFITTABLE_CPU_VCPUS).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    // Class Y: one small, fitting session, queued after both X members.
    enqueue(&meta, y1, y1_spec, y1_mem, y1_cpu).await;

    queue_scanner::run_once(&QueueScannerConfig::default(), &state)
        .await
        .expect("run_once");

    // Within-class FIFO-stop preserved: x1 (the class head) doesn't fit,
    // so x2 (same class, behind it) is never even attempted this sweep —
    // identical to the pre-partition single-class behavior.
    assert_eq!(
        meta.get_session(x1).await.unwrap().status,
        SessionState::Queued
    );
    assert_eq!(
        meta.get_session(x2).await.unwrap().status,
        SessionState::Queued
    );
    assert!(!has_status_change_to(&meta, x2, "pending").await);

    // Cross-class fairness: y1 (a different, smaller, fitting class)
    // still places in the SAME sweep despite class X's block.
    assert!(
        has_status_change_to(&meta, y1, "pending").await,
        "a smaller, fitting class must place in the same sweep even though \
         an unrelated class's head is stuck"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn create_origin_timeout_fails_session_and_records_wait() {
    let Some((meta, state, _database_url)) = setup(false).await else {
        return;
    };

    let sid = SessionId::new();
    enqueue(&meta, sid, spec(), UNFITTABLE_MEM_MIB, UNFITTABLE_CPU_VCPUS).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let cfg = QueueScannerConfig {
        timeout: Duration::from_millis(1),
        ..QueueScannerConfig::default()
    };
    queue_scanner::run_once(&cfg, &state)
        .await
        .expect("run_once");

    let row = meta.get_session(sid).await.unwrap();
    assert_eq!(
        row.status,
        SessionState::Failed,
        "a create-origin session past its timeout must fail, not stay queued forever"
    );
    let events = meta
        .list_session_events_since(sid, -1, 100)
        .await
        .expect("events");
    let timeout_event = events
        .iter()
        .find(|e| e.kind == "queue_timeout")
        .expect("a queue_timeout event must be recorded (the queue-wait metric's outcome path)");
    let waited = timeout_event.payload["waited_secs"]
        .as_i64()
        .expect("waited_secs is a number");
    assert!(waited >= 0, "recorded wait must be non-negative");
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn resume_origin_timeout_returns_to_idle() {
    let Some((meta, state, _database_url)) = setup(false).await else {
        return;
    };

    // Drive a fresh session to Idle via the legal FSM chain (no real boot
    // needed — transition_session is a pure DB flip), then park it back
    // in the queue as a resume.
    let sid = meta.create_session(spec()).await.expect("create");
    meta.transition_session(sid, SessionState::Created)
        .await
        .expect("Pending->Created");
    meta.transition_session(sid, SessionState::Active)
        .await
        .expect("Created->Active");
    meta.transition_session(sid, SessionState::Idle)
        .await
        .expect("Active->Idle");
    meta.enqueue_session_resume(sid)
        .await
        .expect("enqueue resume");
    // Nothing in the fleet can satisfy a resume-origin session with this
    // large a budget — but `enqueue_session_resume` doesn't carry budgets
    // (resume re-derives them at dequeue time via the session's live disk
    // manifest / prior reservation, out of scope here); what matters for
    // THIS test is only that resume-origin timeout returns to `Idle`, not
    // `Failed`. An empty fleet (no ready host at all) guarantees `Some(false)`
    // from `resume_has_capacity` well before any timeout would even need to
    // fire — so instead directly force the timeout path with a 1ms timeout.
    let cfg = QueueScannerConfig {
        timeout: Duration::from_millis(1),
        ..QueueScannerConfig::default()
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    queue_scanner::run_once(&cfg, &state)
        .await
        .expect("run_once");

    let row = meta.get_session(sid).await.unwrap();
    assert_eq!(
        row.status,
        SessionState::Idle,
        "a resume-origin session past its timeout returns to Idle (durable, retryable), not Failed"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn notify_placement_changed_fires_at_every_site() {
    let Some(meta) = connect().await else { return };
    let database_url = std::env::var("ENGRAM_TEST_DATABASE_URL").unwrap();
    let store = engram_postgres::PostgresStore::connect(&database_url)
        .await
        .expect("connect raw store");

    let mut listener = sqlx::postgres::PgListener::connect(&database_url)
        .await
        .expect("listener connect");
    listener
        .listen("placement_changed")
        .await
        .expect("listen placement_changed");
    // Give the listener a moment to settle on the channel before the
    // first NOTIFY (matches the `session_events` precedent in
    // `ha_listener.rs`).
    tokio::time::sleep(Duration::from_millis(150)).await;

    // `placement_changed` is a GLOBAL channel: other live-pg tests running
    // concurrently in this same nextest invocation (each its own process,
    // sharing this one dev Postgres) also write to the 5 notify sites and
    // land noise on this listener. Loop past anything that isn't the
    // reason we're expecting instead of asserting strict message-N
    // ordering, so this test only fails if OUR expected reason never
    // shows up within the deadline.
    async fn wait_for_reason(listener: &mut sqlx::postgres::PgListener, expected: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let notification = tokio::time::timeout(remaining, listener.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("timed out waiting for placement_changed(\"{expected}\")")
                })
                .expect("listener stayed open");
            if notification.payload() == expected {
                return;
            }
            // Noise from a concurrently-running test on the shared channel.
        }
    }

    // 1. reserve_and_persist_create's Queued disposition (issue #535 (b);
    //    formerly `enqueue_session_create`) → "enqueued".
    let sid = SessionId::new();
    enqueue(&meta, sid, spec(), 4096, 2).await;
    wait_for_reason(&mut listener, "enqueued").await;

    // 2. upsert_host → "host_upserted".
    let host = seed_ready_host(&meta, 16_384, 8, &[]).await;
    // `seed_ready_host` calls upsert_host then touch_host_heartbeat;
    // only the former fires this NOTIFY.
    wait_for_reason(&mut listener, "host_upserted").await;

    // 3. set_host_cordoned(false) → "host_uncordoned" (cordon itself does
    //    NOT notify — only the uncordon direction does).
    meta.set_host_cordoned(host, true).await.expect("cordon");
    meta.set_host_cordoned(host, false).await.expect("uncordon");
    wait_for_reason(&mut listener, "host_uncordoned").await;

    // 4. transition_session leaving a reserving state → "session_freed".
    let placed = meta
        .place_queued_session(sid, 4096, 2, &[host], 0)
        .await
        .expect("place")
        .expect("fits");
    assert_eq!(placed, host);
    let prev = meta
        .transition_session(sid, SessionState::Failed)
        .await
        .expect("Pending->Failed");
    assert_eq!(prev, SessionState::Pending);
    wait_for_reason(&mut listener, "session_freed").await;

    // 5. delete_pending_session → "pending_deleted".
    let sid2 = SessionId::new();
    enqueue(&meta, sid2, spec(), 4096, 2).await;
    wait_for_reason(&mut listener, "enqueued").await;
    // `place_queued_session` is NOT a notify site (only the 5 sites in
    // `notify_placement_changed`'s doc comment are), so no drain needed here.
    meta.place_queued_session(sid2, 4096, 2, &[host], 0)
        .await
        .expect("place2")
        .expect("fits2");
    store
        .delete_pending_session(sid2)
        .await
        .expect("delete pending");
    wait_for_reason(&mut listener, "pending_deleted").await;
}

/// Issue #537/PR #559 (T6): the negative twin of
/// `notify_placement_changed_fires_at_every_site` above.
/// `resume_origin_enqueue_requires_idle` already pins the ROW-level
/// no-op (a session that isn't `idle` — already past it, e.g. `queued` —
/// leaves `enqueue_session_resume` a no-op); this pins the NOTIFY side:
/// the guard in `enqueue_session_resume` (`if n > 0`) means the no-op
/// path must NOT fire `placement_changed`, or every replica's scanner
/// wakes into a full fleet sweep for a resume that changed nothing.
///
/// Same LISTEN harness as the positive test, but a bounded NEGATIVE wait
/// instead of waiting for an expected payload. Like every other test in
/// this file that shares the global `placement_changed` channel, this
/// depends on the suite running with `--test-threads=1` (ci.yml's
/// "Postgres-gated ignored tests" step, and this file's own module
/// comment above) for correctness, not just determinism — a concurrently
/// running sibling test's own legitimate NOTIFY could otherwise land in
/// the window and produce a false failure.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn enqueue_session_resume_noop_does_not_notify_placement_changed() {
    let Some(meta) = connect().await else { return };
    let database_url = std::env::var("ENGRAM_TEST_DATABASE_URL").unwrap();

    let mut listener = sqlx::postgres::PgListener::connect(&database_url)
        .await
        .expect("listener connect");
    listener
        .listen("placement_changed")
        .await
        .expect("listen placement_changed");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Seed a `queued` (not `idle`) session — `enqueue_session_resume` is
    // gated on `status='idle'`, so calling it on this row is the no-op
    // path under test.
    let sid = SessionId::new();
    enqueue(&meta, sid, spec(), 4096, 2).await;

    // The seed itself is a REAL notify site ("enqueued") — drain it
    // first so it can't be mistaken for a notify fired by the no-op
    // call below.
    let seed_notify = tokio::time::timeout(Duration::from_secs(10), listener.recv())
        .await
        .expect("timed out waiting for the seed's own enqueued notify")
        .expect("listener stayed open");
    assert_eq!(seed_notify.payload(), "enqueued");

    meta.enqueue_session_resume(sid)
        .await
        .expect("resume enqueue no-op");
    let row = meta.get_session(sid).await.unwrap();
    assert_eq!(
        row.status,
        SessionState::Queued,
        "no-op must not move the row (still the create-origin queued row)"
    );

    // Bounded negative wait: generous enough that a real notify from the
    // call under test would have landed by now, short enough to keep
    // this test fast.
    match tokio::time::timeout(Duration::from_secs(1), listener.recv()).await {
        Err(_) => {} // timed out — no notify arrived; the no-op fired nothing.
        Ok(Ok(n)) => panic!(
            "enqueue_session_resume's no-op path must not fire placement_changed, got: {:?}",
            n.payload()
        ),
        Ok(Err(e)) => panic!("listener died: {e}"),
    }
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn scanner_wakes_on_notify_and_places_within_the_wake_not_the_fallback() {
    let Some((meta, state, database_url)) = setup(false).await else {
        return;
    };

    // `queued_id` is placed via the real scanner (`place_create`), so — per
    // the digest-gate comment in `hol_break_is_per_class_not_global` — it
    // needs its own live `enabled_images` row, and the host must advertise
    // that digest in `ready_images`. `filler` reserves directly via
    // `reserve_placement` (bypassing `place_create`), so its own `spec()`
    // image needs no row.
    let queued_spec = spec();
    let queued_digest = seed_enabled_image(&meta, &queued_spec.image).await;

    // A dedicated, distinctively oversized host so no OTHER
    // concurrently-running test's host could accidentally satisfy the
    // queued session below before we free the filler — that would defeat
    // the "genuinely cannot fit yet" premise.
    let host = seed_ready_host(&meta, SCANNER_WAKE_MEM_MIB as u64, 64, &[queued_digest]).await;

    // Fill the ENTIRE host with a direct reservation (Pending reserves
    // host memory) so the queued session below genuinely cannot fit
    // until the filler is released.
    let filler = SessionId::new();
    let filler_host = reserve(
        &state.services.meta,
        filler,
        &spec(),
        SCANNER_WAKE_MEM_MIB,
        1,
        &[host],
        0,
    )
    .await
    .expect("filler fits exactly");
    assert_eq!(filler_host, host);

    let queued_id = SessionId::new();
    enqueue(&meta, queued_id, queued_spec, SCANNER_WAKE_MEM_MIB, 1).await;

    // Spawn the REAL scanner + pg_listener with a deliberately long
    // fallback poll (well beyond this test's deadline below) so a
    // placement observed within the deadline can only be explained by
    // the NOTIFY wake, not the poll fallback.
    let wake = Arc::new(tokio::sync::Notify::new());
    let cfg = QueueScannerConfig {
        poll_interval: Duration::from_secs(120),
        ..QueueScannerConfig::default()
    };
    let _scanner = queue_scanner::spawn(cfg, state.clone(), wake.clone());
    let _listener = engram_coordinator::pg_listener::spawn(
        database_url.clone(),
        meta.clone(),
        state.events.clone(),
        state.host_registry.clone(),
        state.integrations.clone(),
        state.boot_bundles.clone(),
        wake,
        std::sync::Arc::new(tokio::sync::Notify::new()),
        std::sync::Arc::new(tokio::sync::Notify::new()),
    );
    // Let both tasks reach their first `select!` / `LISTEN` before we
    // fire the freeing event.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Release the filler — frees the whole host and fires the
    // `session_freed` NOTIFY that should wake the parked scanner.
    state
        .services
        .meta
        .transition_session(filler, SessionState::Failed)
        .await
        .expect("free the filler");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if has_status_change_to(&meta, queued_id, "pending").await {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "queued session was not placed within 10s of the freeing NOTIFY — \
             the 120s poll fallback proves this isn't just a slow poll",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
