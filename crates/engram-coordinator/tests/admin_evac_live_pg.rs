//! Live-Postgres integration tests for ADR 0018 session evacuation.
//!
//! Exercises `evacuate_dead_source` (dead/degraded source) against a
//! real PG + the in-memory HostRegistry + mock HostClient backends.
//! The PG side is what these tests really exercise — the rebind of
//! `sessions.host_id` / `sessions.sandbox_id` via the
//! `assign_session_*` and `transition_session` calls must work
//! against the real legality-table-enforcing implementation, not a
//! mock.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. CI wires this into the
//! Postgres-gated-ignored lane alongside `admin_chunk_gc_live_pg`.
//!
//! Coverage:
//! - `evacuate_dead_source_with_snapshot_uses_recorded_manifests`
//!   — restores from a recorded snapshot's disk+memory manifests
//!   when only the snapshot is available. Loss=None.
//! - `evacuate_dead_source_disk_only_records_memory_loss` — when
//!   only `sessions.live_disk_manifest_*` is set (no snapshot row),
//!   the receipt carries `EvacLoss::Memory{reason: "source-dead-..."}`.
//! - `evacuate_dead_source_no_state_returns_no_recoverable` — both
//!   manifests absent → typed error; PG row stays at HostLost so the
//!   caller (the `evac_resumer` scanner, fed by operator drain) can
//!   surface it. (As of ADR 0045 Phase A the dead-host detector no
//!   longer calls this — it routes recoverable sessions to Idle and
//!   the rest to Dead directly; this primitive is drain-only now.)

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef as ChunkManifestRef};
use engram_coordinator::evacuation::{evacuate_dead_source, EvacError};
use engram_coordinator::host_registry::HostRegistry;
use engram_core::traits::{HarnessDial, HostClient, MetadataStore};
use engram_core::types::evacuation::EvacLoss;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::snapshot::{SnapshotMetadata, SnapshotRecord};
use engram_core::{HostId, SandboxError, SandboxId, SessionId, SnapshotId};
use engram_storage_local::LocalBlobStorage;
use parking_lot::Mutex;

struct TestRig {
    meta: Arc<dyn MetadataStore>,
    chunk_store: ChunkStore,
    _blob_dir: tempfile::TempDir,
}

async fn rig() -> Option<TestRig> {
    let database_url = match std::env::var("ENGRAM_TEST_DATABASE_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skipping: ENGRAM_TEST_DATABASE_URL not set. Run with `just db-up` first; \
                 default URL is postgres://engram:engram@localhost:5435/engram",
            );
            return None;
        }
    };
    // ADR 0047: placement reads the GLOBAL hosts table now, so tests in
    // this binary can no longer share a database — a sibling test's
    // fresh host row would be a legal pick. Give each rig its own
    // database, created off the configured URL. (Leaked test databases
    // are fine: CI's Postgres is ephemeral, and local dev reuses names
    // rarely enough to not matter.)
    let admin = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect postgres (admin)");
    let db_name = format!("engram_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!(r#"CREATE DATABASE "{db_name}""#))
        .execute(&admin)
        .await
        .expect("create per-test database");
    let base = database_url
        .rsplit_once('/')
        .map(|(b, _)| b)
        .expect("database url has a path");
    let test_url = format!("{base}/{db_name}");
    let store = engram_postgres::PostgresStore::connect(&test_url)
        .await
        .expect("connect postgres");
    store.migrate().await.expect("migrate");

    let blob_dir = tempfile::tempdir().expect("tempdir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> =
        Arc::new(LocalBlobStorage::new(blob_dir.path().to_path_buf()));
    let chunk_store = ChunkStore::new(blob);

    let pg = Arc::new(store);
    Some(TestRig {
        meta: pg as Arc<dyn MetadataStore>,
        chunk_store,
        _blob_dir: blob_dir,
    })
}

/// FakeBackend: returns deterministic ids. Mirrors the shape used in
/// evacuation.rs's unit tests but lifted here so the integration tests
/// can register it against the real HostRegistry. Production
/// HostClients require a host process to wire up; for the
/// PG-state-flow tests that's noise.
#[derive(Default)]
struct FakeBackend {
    next_restore_id: Mutex<Option<SandboxId>>,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn set_restore_id(&self, id: SandboxId) {
        *self.next_restore_id.lock() = Some(id);
    }
}

#[async_trait]
impl HostClient for FakeBackend {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        Ok(SandboxId::new())
    }
    async fn destroy(
        &self,
        _id: SandboxId,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(vec![])
    }
    async fn probe_sandbox(
        &self,
        _id: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        unimplemented!()
    }
    async fn exec_stream(
        &self,
        _id: SandboxId,
        _cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        unreachable!()
    }
    async fn snapshot(
        &self,
        _id: SandboxId,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        Ok(SnapshotMetadata {
            id: SnapshotId::new(),
            size_bytes: 1024,
            created_at: Utc::now(),
            image_version: "test".into(),
            disk_manifest: None,
            memory_manifest: None,
            base_memory_manifest: None,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            paused_at: None,
        })
    }
    async fn commit_snapshot(
        &self,
        _id: SandboxId,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn abort_snapshot(
        &self,
        _id: SandboxId,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn restore(
        &self,
        _md: SnapshotMetadata,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        // Tests always preload an id; the fallback is defensive.
        // Match-based dispatch avoids clippy's unwrap_or_default lint
        // (Default for SandboxId would mint a nil UUID, masking
        // test bugs) and unwrap_or_else's redundant_closure lint.
        Ok(match *self.next_restore_id.lock() {
            Some(id) => id,
            None => SandboxId::new(),
        })
    }
    async fn start_agent(
        &self,
        _id: SandboxId,
        _agent: engram_core::types::sandbox::AgentSpec,
        _policy: engram_core::types::egress::SessionEgressPolicy,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn apply_egress_policy(
        &self,
        _policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }
    async fn bind_session(
        &self,
        _session_id: SessionId,
        _sandbox_id: SandboxId,
        _binding_epoch: u64,
    ) {
    }
    async fn unbind_session(&self, _session_id: SessionId) {}
    async fn send_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
        _text: String,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }
}

/// Upsert a `hosts` row so the `sessions.host_id` foreign key is
/// satisfied. PG enforces the FK; the unit tests bypass it via the
/// in-memory FakeMeta, but live-PG needs the parent row.
///
/// Hostname is unique per `host_id` because `upsert_host`'s
/// `ON CONFLICT (hostname)` would otherwise update an existing
/// fixed-name row in place — leaving the new host_id unindexed and
/// re-firing the FK violation we're trying to avoid. The host_id
/// suffix guarantees collision-free seeding across tests in the
/// shared PG instance.
async fn ensure_host_row(meta: &Arc<dyn MetadataStore>, host_id: HostId, label: &str) {
    use engram_core::types::host::HostRecord;
    use engram_core::types::{HostCapacity, HostMetadata, HostStatus};
    meta.upsert_host(HostRecord {
        id: host_id,
        hostname: format!("{label}-{host_id}"),
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

/// Seed a host row with an explicit `status` + `last_heartbeat_at` so a test
/// can stage stale / draining hosts for the dead-host detector's
/// `list_stale_hosts` query.
async fn seed_host_with(
    meta: &Arc<dyn MetadataStore>,
    host_id: HostId,
    label: &str,
    status: engram_core::types::HostStatus,
    last_heartbeat_at: chrono::DateTime<Utc>,
) {
    use engram_core::types::host::HostRecord;
    use engram_core::types::{HostCapacity, HostMetadata};
    meta.upsert_host(HostRecord {
        id: host_id,
        hostname: format!("{label}-{host_id}"),
        cloud_metadata: HostMetadata::default(),
        capacity: HostCapacity {
            total_gb: 100,
            used_gb: 10,
            total_mib: 65_536,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        status,
        last_heartbeat_at,
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

/// Mint a fresh session row at Active status with `host_id` and
/// `sandbox_id` bound. Returns the session_id for follow-up queries.
/// Inserts the `hosts` row first to keep PG's FK happy.
async fn seed_active_session(
    meta: &Arc<dyn MetadataStore>,
    host_id: HostId,
    sandbox_id: SandboxId,
) -> SessionId {
    ensure_host_row(meta, host_id, "source").await;
    let session_id = meta
        .create_session(SessionSpec {
            image: format!("ghcr.io/test/img:t-{}", uuid::Uuid::new_v4()),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create_session");
    meta.assign_session_host(session_id, Some(host_id))
        .await
        .expect("assign_session_host");
    meta.assign_session_sandbox(session_id, Some(sandbox_id))
        .await
        .expect("assign_session_sandbox");
    // create_session lands at Pending; create_session_created is the
    // production path that goes straight to Created. Drive the
    // sequence so we end at Active for our tests.
    meta.transition_session(session_id, SessionState::Created)
        .await
        .expect("Pending → Created");
    meta.transition_session(session_id, SessionState::Active)
        .await
        .expect("Created → Active");
    session_id
}

async fn seed_manifest(store: &ChunkStore, kind: ManifestKind, label: &str) -> ChunkManifestRef {
    use engram_chunk_store::{ChunkRef, Manifest};
    let bytes = format!("{label}-{}", uuid::Uuid::new_v4()).into_bytes();
    let hash = store.put_chunk(&bytes).await.expect("put chunk");
    let mut manifest = Manifest::empty(kind, bytes.len() as u64);
    manifest.chunks.push(ChunkRef { offset: 0, hash });
    let r = ChunkManifestRef::new();
    store
        .put_manifest(r, &manifest)
        .await
        .expect("put manifest");
    r
}

fn proto_to_core_manifest(r: ChunkManifestRef) -> ManifestRef {
    ManifestRef {
        manifest_id: r.manifest_id,
        version: r.version,
    }
}

/// ADR 0028 Fix B: the cold-boot spec a disk-only recovery rides (in
/// prod, derived from the enabled image via `resolve_cold_boot_spec`).
fn test_cold_boot_spec() -> SandboxSpec {
    SandboxSpec {
        image: "ghcr.io/test/img:t".into(),
        rootfs_source: None,
        image_uri: Some("ghcr.io/test/img:t".into()),
        rootfs_manifest: None,
        cpu: engram_core::types::sandbox::CpuLimit { vcpus: 2 },
        memory: engram_core::types::sandbox::MemoryLimit { max_mib: 4096 },
        disk: engram_core::types::sandbox::DiskLimit { max_gib: 20 },
        ttl: None,
        env: Default::default(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    }
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn evacuate_dead_source_with_snapshot_uses_recorded_manifests() {
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();
    let registry = Arc::new(HostRegistry::new(meta.clone()));

    let dead_source = HostId::new();
    let target_host = HostId::new();
    let target_be = FakeBackend::new();
    let new_sandbox = SandboxId::new();
    target_be.set_restore_id(new_sandbox);
    registry.register(target_host, target_be.clone());

    let session_id = seed_active_session(&meta, dead_source, SandboxId::new()).await;
    ensure_host_row(&meta, target_host, "target").await;
    // Simulate dead_host.rs's first-stage flip: Active → HostLost.
    meta.transition_session(session_id, SessionState::HostLost)
        .await
        .expect("Active → HostLost");

    // Record a recoverable snapshot for this session.
    let disk = seed_manifest(&rig.chunk_store, ManifestKind::Disk, "snap-disk").await;
    let memory = seed_manifest(&rig.chunk_store, ManifestKind::Memory, "snap-mem").await;
    meta.record_snapshot(SnapshotRecord {
        id: SnapshotId::new(),
        session_id: Some(session_id),
        host_id: None,
        image_version: "test".into(),
        size_bytes: 1024,
        created_at: Utc::now(),
        last_accessed_at: Utc::now(),
        disk_manifest: Some(proto_to_core_manifest(disk)),
        memory_manifest: Some(proto_to_core_manifest(memory)),
        recoverable: true,
        aux_bundles: vec![],
        events_cursor: None,
        fc_snapshot_version: None,
    })
    .await
    .expect("record snapshot");

    let session = meta.get_session(session_id).await.expect("get session");
    let snapshot = meta
        .latest_snapshot_for_session(session_id)
        .await
        .expect("latest_snapshot lookup")
        .expect("snapshot present");

    let receipt = evacuate_dead_source(
        &registry,
        &meta,
        session,
        Some(snapshot),
        None,
        None,
        None,
        engram_core::traits::SessionFence::unfenced(),
    )
    .await
    .expect("dead-source evac succeeds");
    assert_eq!(receipt.new_host_id, target_host);
    assert_eq!(receipt.new_sandbox_id, new_sandbox);
    assert_eq!(receipt.loss, EvacLoss::None);

    let after = meta.get_session(session_id).await.expect("get session");
    assert_eq!(after.status, SessionState::Created);
    assert_eq!(after.host_id, Some(target_host));
    assert_eq!(after.sandbox_id, Some(new_sandbox));
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn evacuate_dead_source_disk_only_records_memory_loss() {
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();
    let registry = Arc::new(HostRegistry::new(meta.clone()));

    let dead_source = HostId::new();
    let target_host = HostId::new();
    let target_be = FakeBackend::new();
    target_be.set_restore_id(SandboxId::new());
    registry.register(target_host, target_be);

    let old_sandbox = SandboxId::new();
    let session_id = seed_active_session(&meta, dead_source, old_sandbox).await;
    ensure_host_row(&meta, target_host, "target").await;

    // Set live_disk_manifest_*. The update is sandbox-id-gated so we
    // pass the current binding.
    let live_disk = seed_manifest(&rig.chunk_store, ManifestKind::Disk, "live-disk").await;
    meta.update_live_disk_manifest(session_id, old_sandbox, proto_to_core_manifest(live_disk))
        .await
        .expect("update_live_disk_manifest");

    meta.transition_session(session_id, SessionState::HostLost)
        .await
        .expect("Active → HostLost");

    let session = meta.get_session(session_id).await.expect("get session");
    // ADR 0028 Fix B: disk-only recovery is a cold boot — the caller
    // supplies the boot spec (in prod, derived from the enabled image
    // via `resolve_cold_boot_spec`).
    let receipt = evacuate_dead_source(
        &registry,
        &meta,
        session,
        None,
        Some(test_cold_boot_spec()),
        None,
        None,
        engram_core::traits::SessionFence::unfenced(),
    )
    .await
    .expect("disk-only evac succeeds");
    match &receipt.loss {
        EvacLoss::Memory { reason } => {
            assert_eq!(reason, "source-dead-no-snapshot");
        }
        other => panic!("expected Memory loss, got {other:?}"),
    }

    let after = meta.get_session(session_id).await.expect("get session");
    assert_eq!(after.status, SessionState::Created);
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn evacuate_dead_source_no_state_returns_no_recoverable() {
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();
    let registry = Arc::new(HostRegistry::new(meta.clone()));

    let dead_source = HostId::new();
    let target_host = HostId::new();
    registry.register(target_host, FakeBackend::new());

    let session_id = seed_active_session(&meta, dead_source, SandboxId::new()).await;
    meta.transition_session(session_id, SessionState::HostLost)
        .await
        .expect("Active → HostLost");

    let session = meta.get_session(session_id).await.expect("get session");
    let result = evacuate_dead_source(
        &registry,
        &meta,
        session,
        None,
        None,
        None,
        None,
        engram_core::traits::SessionFence::unfenced(),
    )
    .await;
    assert!(matches!(result, Err(EvacError::NoRecoverableState)));

    // PG row sits at HostLost — the caller routes it to Dead next.
    let after = meta.get_session(session_id).await.expect("get session");
    assert_eq!(after.status, SessionState::HostLost);
}

// ---------------------------------------------------------------------
// ADR 0018 commit 12j — live-PG tests for the new evac_resumer
// scanner primitives. These run in CI's Postgres-gated lane (same
// `ENGRAM_TEST_DATABASE_URL` requirement) and pin the load-bearing
// behaviour of the async-evac state machine.
// ---------------------------------------------------------------------

/// Migration 0037: `evac_attempts` column exists, defaults to 0, and
/// the partial index `idx_sessions_evacuating` is created. Pins the
/// schema so a future migration that drops/renames either surfaces
/// here, not at runtime when the scanner's sweep query 500s.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn migration_0037_landed_evac_attempts_column_and_index() {
    let Some(rig) = rig().await else { return };
    let database_url = std::env::var("ENGRAM_TEST_DATABASE_URL").unwrap();
    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();

    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT column_name, data_type, column_default \
         FROM information_schema.columns \
         WHERE table_name = 'sessions' AND column_name = 'evac_attempts'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    let (col, ty, default) = row.expect("evac_attempts column must exist after migration 0037");
    assert_eq!(col, "evac_attempts");
    assert_eq!(ty, "integer");
    assert!(
        default.as_deref().unwrap_or("").starts_with('0'),
        "evac_attempts default must be 0, got {default:?}"
    );

    // Verify the CHECK constraint accepts 'evacuating' — without
    // this an attempt to UPDATE sessions SET status='evacuating'
    // 500s with a constraint violation (caught on dev-vm; that
    // failure mode is exactly what the migration's first ALTER
    // block guards against).
    let idx: Option<(String,)> = sqlx::query_as(
        "SELECT indexname FROM pg_indexes WHERE indexname = 'idx_sessions_evacuating'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(idx.is_some(), "idx_sessions_evacuating must exist");

    drop(rig);
}

/// `list_evacuating_sessions` returns the right set + their
/// `evac_attempts` values; `bump_evac_attempts` is atomic +1
/// RETURNING; `transition_session(Evacuating)` resets the counter.
/// All three are load-bearing for the scanner's sweep loop.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn evac_attempts_primitives_round_trip() {
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();

    let host_id = HostId::new();
    let sandbox_id = SandboxId::new();
    let session_id = seed_active_session(&meta, host_id, sandbox_id).await;

    // Active → Evacuating (legal, sets counter to 0)
    meta.transition_session(session_id, SessionState::Evacuating)
        .await
        .expect("Active → Evacuating");

    let candidates = meta
        .list_evacuating_sessions()
        .await
        .expect("list_evacuating_sessions");
    let entry = candidates
        .iter()
        .find(|(s, _)| s.id == session_id)
        .expect("our session in the candidate list");
    assert_eq!(entry.1, 0, "counter starts at 0 after Evacuating entry");

    // bump returns new value; second bump returns 2.
    let v1 = meta.bump_evac_attempts(session_id).await.unwrap();
    let v2 = meta.bump_evac_attempts(session_id).await.unwrap();
    assert_eq!(v1, 1);
    assert_eq!(v2, 2);

    // list reflects the latest bump.
    let candidates = meta.list_evacuating_sessions().await.unwrap();
    let after_bump = candidates.iter().find(|(s, _)| s.id == session_id).unwrap();
    assert_eq!(after_bump.1, 2, "list reads back the bumped count");

    // Transition out (Evacuating → Idle) — counter NOT reset (only
    // re-entry into Evacuating resets, per migration 0037's CASE).
    meta.transition_session(session_id, SessionState::Idle)
        .await
        .expect("Evacuating → Idle (budget-exhaustion fallback shape)");
    let candidates = meta.list_evacuating_sessions().await.unwrap();
    assert!(
        candidates.iter().all(|(s, _)| s.id != session_id),
        "session no longer Evacuating should drop out of the sweep"
    );

    // Re-enter Evacuating from Idle. Wait — Idle → Evacuating isn't
    // legal directly. The supported re-entry path goes through
    // Active. Drive Idle → Created → Active → Evacuating to exercise
    // the legality + the counter-reset on entry.
    meta.assign_session_sandbox(session_id, Some(SandboxId::new()))
        .await
        .unwrap();
    meta.transition_session(session_id, SessionState::Created)
        .await
        .expect("Idle → Created");
    meta.transition_session(session_id, SessionState::Active)
        .await
        .expect("Created → Active");
    meta.transition_session(session_id, SessionState::Evacuating)
        .await
        .expect("Active → Evacuating (second drain)");

    let candidates = meta.list_evacuating_sessions().await.unwrap();
    let on_reentry = candidates.iter().find(|(s, _)| s.id == session_id).unwrap();
    assert_eq!(
        on_reentry.1, 0,
        "re-entry into Evacuating must reset evac_attempts to 0"
    );
}

/// Issue #215 (live-PG): the actual `sessions.missing_strikes` SQL must
/// reset on a sandbox re-key. Accrue `grace_ticks - 1` strikes against
/// SB1 via `apply_missing_sandbox_strikes`, then rebind to SB2 via the
/// production binding writers and assert the column is back at 0 — so a
/// single subsequent missing tick does NOT cross the threshold. Before
/// the fix, `assign_session_sandbox` / `rebind_session` left the column
/// untouched and the very next missing tick flipped a healthy session.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn missing_strikes_reset_on_sandbox_rekey() {
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();
    let grace = engram_coordinator::reconcile::DEFAULT_GRACE_TICKS as i32;

    let host_a = HostId::new();
    let sb1 = SandboxId::new();
    let session_id = seed_active_session(&meta, host_a, sb1).await;

    // Accrue grace-1 strikes against the current binding (SB1 missing
    // for grace-1 consecutive ticks). None of these cross the
    // threshold, so nothing flips.
    for _ in 0..(grace - 1) {
        let flipped = meta
            .apply_missing_sandbox_strikes(&[], &[session_id], grace)
            .await
            .expect("apply_missing_sandbox_strikes");
        assert!(flipped.is_empty(), "below threshold → no flip");
    }

    // --- Path 1: assign_session_sandbox(Some) rebind resets strikes.
    let sb2 = SandboxId::new();
    meta.assign_session_sandbox(session_id, Some(sb2))
        .await
        .expect("rebind to SB2");
    let flipped = meta
        .apply_missing_sandbox_strikes(&[], &[session_id], grace)
        .await
        .expect("apply after rebind");
    assert!(
        flipped.is_empty(),
        "the first missing tick after a sandbox re-key must be strike 1 of a fresh window, \
         not the grace-th — stale strikes leaked across the rebind"
    );

    // --- Path 2: rebind_session (host+sandbox in one UPDATE) resets too.
    // Re-accrue up to grace-1 (we're at 1 from the tick above; bump to grace-1).
    for _ in 0..(grace - 2) {
        let flipped = meta
            .apply_missing_sandbox_strikes(&[], &[session_id], grace)
            .await
            .expect("re-accrue");
        assert!(flipped.is_empty());
    }
    let host_b = HostId::new();
    ensure_host_row(&meta, host_b, "rekey-dest").await;
    let sb3 = SandboxId::new();
    meta.rebind_session(session_id, host_b, sb3)
        .await
        .expect("rebind_session to SB3 on host B");
    let flipped = meta
        .apply_missing_sandbox_strikes(&[], &[session_id], grace)
        .await
        .expect("apply after rebind_session");
    assert!(
        flipped.is_empty(),
        "rebind_session must also reset the strike streak"
    );

    // --- Path 3: unbind (assign_session_sandbox(None)) clears strikes.
    // Re-accrue to grace-1, unbind, rebind, then a single miss must not flip.
    for _ in 0..(grace - 2) {
        meta.apply_missing_sandbox_strikes(&[], &[session_id], grace)
            .await
            .expect("re-accrue before unbind");
    }
    meta.assign_session_sandbox(session_id, None)
        .await
        .expect("unbind");
    // Rebind to a fresh sandbox so the row is bound again for the tick.
    let sb4 = SandboxId::new();
    meta.assign_session_sandbox(session_id, Some(sb4))
        .await
        .expect("rebind after unbind");
    let flipped = meta
        .apply_missing_sandbox_strikes(&[], &[session_id], grace)
        .await
        .expect("apply after unbind+rebind");
    assert!(
        flipped.is_empty(),
        "unbind must clear the strike streak so the next binding gets a fresh window"
    );
}

/// ADR 0047: the durable coordinator cordon. `set_host_cordoned` writes
/// the PG bit; `placement::pick_for_session` (reading host rows) must
/// skip the host — from ANY replica (a second registry over the same
/// store sees the same cordon), and the cordon survives heartbeats
/// (`touch_host_heartbeat` never writes the bit).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn durable_cordon_excludes_host_from_placement_on_every_replica() {
    use engram_coordinator::host_registry::HostRegistry;
    use engram_coordinator::placement::{self, ScheduleContext};
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();
    let registry = Arc::new(HostRegistry::new(meta.clone()));
    let replica_b = Arc::new(HostRegistry::new(meta.clone()));

    let cordoned = HostId::new();
    let healthy = HostId::new();
    seed_ready_host(&meta, cordoned, "cordon-target").await;
    seed_ready_host(&meta, healthy, "cordon-peer").await;
    registry.register(cordoned, FakeBackend::new());
    registry.register(healthy, FakeBackend::new());
    replica_b.register(cordoned, FakeBackend::new());
    replica_b.register(healthy, FakeBackend::new());

    // Both hosts available → either can be picked.
    let ctx = ScheduleContext {
        repo: "test/img",
        image_version: "v1",
        snapshot_host: None,
        memory_mib: None,
        cpu_budget_vcpus: None,
        required_image_digest: None,
        exclude_host: None,
        prefer_host: None,
        caps: Default::default(),
    };
    let (first_pick, _) = placement::pick_for_session(meta.as_ref(), &registry, &ctx)
        .await
        .expect("pick succeeds");
    assert!(first_pick == cordoned || first_pick == healthy);

    // Cordon — every replica's picker MUST avoid it.
    meta.set_host_cordoned(cordoned, true)
        .await
        .expect("cordon a rowed host");
    for reg in [&registry, &replica_b] {
        for _ in 0..10 {
            let (picked, _) = placement::pick_for_session(meta.as_ref(), reg, &ctx)
                .await
                .expect("pick succeeds");
            assert_eq!(
                picked, healthy,
                "cordoned host must never be picked (got {picked} after cordon)"
            );
        }
    }

    // A heartbeat must NOT clobber the cordon (the pre-0047 bug).
    meta.touch_host_heartbeat(
        cordoned,
        engram_core::types::host::HostHeartbeat {
            status: engram_core::types::HostStatus::Ready,
            capacity: engram_core::types::HostCapacity {
                total_gb: 0,
                used_gb: 0,
                total_mib: 16_384,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: Default::default(),
            ready_images: Vec::new(),
            current_bundles: Vec::new(),
            total_vcpus: 8,
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
        },
    )
    .await
    .expect("heartbeat");
    let (picked, _) = placement::pick_for_session(meta.as_ref(), &registry, &ctx)
        .await
        .expect("pick succeeds");
    assert_eq!(picked, healthy, "heartbeat must not clear the cordon");

    // Uncordon — the previously-cordoned host is eligible again. Use
    // exclude_host to force the pick deterministically.
    meta.set_host_cordoned(cordoned, false)
        .await
        .expect("uncordon");
    let exclude_healthy_ctx = ScheduleContext {
        exclude_host: Some(healthy),
        ..ctx.clone()
    };
    let (picked, _) = placement::pick_for_session(meta.as_ref(), &registry, &exclude_healthy_ctx)
        .await
        .expect("post-uncordon pick must succeed when healthy host is excluded");
    assert_eq!(
        picked, cordoned,
        "after uncordon, the picker must return the previously-cordoned host"
    );

    // Unknown host id → NotFound (admin endpoint maps to 404).
    assert!(matches!(
        meta.set_host_cordoned(HostId::new(), true).await,
        Err(engram_core::MetaError::NotFound)
    ));
}

/// Seed a fresh-heartbeat `ready` host row so ADR 0047 placement (which
/// reads PG) can schedule onto it.
async fn seed_ready_host(
    meta: &Arc<dyn engram_core::traits::MetadataStore>,
    id: HostId,
    hostname: &str,
) {
    use engram_core::types::host::HostRecord;
    meta.upsert_host(HostRecord {
        id,
        hostname: hostname.into(),
        cloud_metadata: Default::default(),
        capacity: engram_core::types::HostCapacity {
            total_gb: 0,
            used_gb: 0,
            total_mib: 16_384,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        status: engram_core::types::HostStatus::Ready,
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
    .expect("seed host row");
}

/// ADR 0044 K3 amendment: the dead-host detector must NOT strike out a
/// `draining` host — it's operator-managed (mid image-roll, where K2 reattach
/// keeps its VMs alive across the pod-swap heartbeat gap, or mid node-removal).
/// So `list_stale_hosts` returns only stale `ready` hosts.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn list_stale_hosts_excludes_draining_hosts() {
    use engram_core::types::HostStatus;
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();

    let stale = Utc::now() - chrono::Duration::seconds(120);
    let fresh = Utc::now();
    let stale_ready = HostId::new();
    let stale_draining = HostId::new();
    let fresh_ready = HostId::new();
    seed_host_with(&meta, stale_ready, "stale-ready", HostStatus::Ready, stale).await;
    seed_host_with(
        &meta,
        stale_draining,
        "stale-drain",
        HostStatus::Draining,
        stale,
    )
    .await;
    seed_host_with(&meta, fresh_ready, "fresh-ready", HostStatus::Ready, fresh).await;

    let stale_ids: Vec<HostId> = meta
        .list_stale_hosts(60)
        .await
        .expect("list_stale_hosts")
        .into_iter()
        .map(|h| h.id)
        .collect();

    assert!(
        stale_ids.contains(&stale_ready),
        "a stale READY host is a strike-out candidate"
    );
    assert!(
        !stale_ids.contains(&stale_draining),
        "a stale DRAINING host is operator-managed — must be excluded"
    );
    assert!(
        !stale_ids.contains(&fresh_ready),
        "a fresh host is not stale"
    );
}

/// ADR 0047: `apply_missing_sandbox_strikes` semantics on the real SQL —
/// the consecutive-reset behavior that per-pod counters corrupted under
/// round-robin heartbeats. (Ports the pre-0047 in-memory `apply_strikes`
/// unit tests onto the shared `sessions.missing_strikes` column.)
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn missing_sandbox_strikes_are_consecutive_and_shared() {
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();
    let host = HostId::new();
    let sb_a = SandboxId::new();
    let sb_b = SandboxId::new();
    let sid_a = seed_active_session(&meta, host, sb_a).await;
    let sid_b = seed_active_session(&meta, host, sb_b).await;

    // Two missing ticks for A (B present) — no flip yet.
    for _ in 0..2 {
        let flipped = meta
            .apply_missing_sandbox_strikes(&[sid_b], &[sid_a], 3)
            .await
            .expect("strike tick");
        assert!(flipped.is_empty(), "below grace must not flip");
    }
    // A re-appears: resets — the two prior strikes must not carry over.
    let flipped = meta
        .apply_missing_sandbox_strikes(&[sid_a, sid_b], &[], 3)
        .await
        .expect("reset tick");
    assert!(flipped.is_empty());
    // Three consecutive missing ticks now flip A exactly at grace —
    // proving the reset took (2 stale + 1 would have flipped at tick 1).
    for tick in 0..3 {
        let flipped = meta
            .apply_missing_sandbox_strikes(&[sid_b], &[sid_a], 3)
            .await
            .expect("strike tick");
        if tick < 2 {
            assert!(flipped.is_empty(), "tick {tick} must not flip");
        } else {
            assert_eq!(flipped, vec![sid_a], "third consecutive miss flips");
        }
    }
    // The flip reset the counter: the next miss starts from scratch.
    let flipped = meta
        .apply_missing_sandbox_strikes(&[sid_b], &[sid_a], 3)
        .await
        .expect("post-flip tick");
    assert!(flipped.is_empty(), "post-flip counter starts fresh");
    // B was present throughout — untouched. grace=1 flips immediately.
    let flipped = meta
        .apply_missing_sandbox_strikes(&[], &[sid_b], 1)
        .await
        .expect("grace-1 tick");
    assert_eq!(flipped, vec![sid_b], "grace=1 is the no-grace mode");
}

/// ADR 0048 C9: `delete_host` is the operator's immediate-deregister
/// primitive for the scale-down wave. It must REFUSE while any session
/// is still bound (the operator finishes draining first — deleting the
/// row out from under a live session orphans its routing), then succeed
/// once the host is empty, and be idempotent if the row is already gone
/// (the wave driver may re-issue it after a restart). This pins the
/// SQL the `DELETE /api/admin/hosts/:id` handler maps onto.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn delete_host_refuses_bound_then_idempotent() {
    use engram_core::types::session::DeleteHostOutcome;
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();

    let host = HostId::new();
    let sid = seed_active_session(&meta, host, SandboxId::new()).await;

    // An Active session is bound → refuse with the bound count.
    match meta.delete_host(host).await.expect("delete_host") {
        DeleteHostOutcome::SessionsBound(n) => assert_eq!(n, 1, "one Active session bound"),
        other => panic!("expected SessionsBound(1) while a session is Active, got {other:?}"),
    }
    // Row must survive the refusal.
    assert!(
        meta.list_active_hosts()
            .await
            .expect("list hosts")
            .iter()
            .any(|h| h.id == host),
        "a refused delete must leave the host row intact"
    );

    // Move the session out of the bound set (Idle is not counted). Now
    // the host is drainable.
    meta.transition_session(sid, SessionState::Idle)
        .await
        .expect("Active → Idle");

    match meta.delete_host(host).await.expect("delete_host") {
        DeleteHostOutcome::Deleted => {}
        other => panic!("expected Deleted once no session is bound, got {other:?}"),
    }
    assert!(
        !meta
            .list_active_hosts()
            .await
            .expect("list hosts")
            .iter()
            .any(|h| h.id == host),
        "row must be gone after a successful delete"
    );
    // The Idle straggler was detached, not deleted.
    let after = meta.get_session(sid).await.expect("get session");
    assert_eq!(after.host_id, None, "delete_host detaches idle stragglers");

    // Idempotent: deleting an already-gone row is a no-op success (the
    // wave driver re-issues after a restart without a row to find).
    match meta
        .delete_host(host)
        .await
        .expect("delete_host (idempotent)")
    {
        DeleteHostOutcome::Deleted => {}
        other => panic!("a second delete of a gone row must be Deleted, got {other:?}"),
    }
}

/// ADR 0048 C8 (drain don't-strand guard): `placement_preview` is the
/// HARD 2D fit check `drain_host` runs before starting ANY move. If the
/// only survivor (the victim excluded) can't hold the session's budgets,
/// it returns `false` so the drain surfaces a failure instead of parking
/// an Active session Idle on a full fleet. A measured-but-too-small
/// survivor → false; growing it (or its CPU budget) → true. An UNMEASURED
/// survivor (allocatable 0) keeps the soft-fits posture → true.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn drain_dont_strand_guard_blocks_when_no_survivor_fits() {
    use engram_coordinator::placement::{self, ScheduleContext};
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();

    let victim = HostId::new();
    let survivor = HostId::new();
    seed_ready_host(&meta, victim, "drain-victim").await;
    seed_ready_host(&meta, survivor, "drain-survivor").await;

    // Heartbeat the survivor as MEASURED with a small allocatable + a
    // CPU budget. allocatable 4096 MiB; total_vcpus 4 ⇒ CPU budget
    // 4 × overcommit (default 4.0) = 16 vCPU.
    let heartbeat = |alloc_mib: u64, vcpus: u32| {
        let meta = meta.clone();
        async move {
            meta.touch_host_heartbeat(
                survivor,
                engram_core::types::host::HostHeartbeat {
                    status: engram_core::types::HostStatus::Ready,
                    capacity: engram_core::types::HostCapacity {
                        total_gb: 0,
                        used_gb: 0,
                        total_mib: 65_536,
                        used_mib: 0,
                        running_sandboxes: 0,
                    },
                    utilization: engram_core::types::host::HostUtilization {
                        allocatable_mib: alloc_mib,
                        ..Default::default()
                    },
                    ready_images: Vec::new(),
                    current_bundles: Vec::new(),
                    total_vcpus: vcpus,
                    wire_version: engram_protocol::WIRE_VERSION,
                    stages_images: false,
                    capabilities: engram_core::types::host::HostCapabilities::default(),
                },
            )
            .await
            .expect("heartbeat survivor");
        }
    };
    heartbeat(4_096, 4).await;

    // The victim is excluded (it's draining); the survivor is the only
    // candidate left.
    let ctx = ScheduleContext {
        repo: "test/img",
        image_version: "v1",
        snapshot_host: None,
        memory_mib: Some(8_192),
        cpu_budget_vcpus: Some(2),
        required_image_digest: None,
        exclude_host: Some(victim),
        prefer_host: None,
        caps: Default::default(),
    };

    // 8 GiB session, survivor has 4 GiB free → no fit → would strand.
    let fits = placement::placement_preview(meta.as_ref(), &ctx, 8_192, 2)
        .await
        .expect("placement_preview");
    assert!(
        !fits,
        "a 8 GiB session must NOT fit a 4 GiB survivor — the guard blocks the drain"
    );

    // Grow the survivor's RAM → now it fits both dims.
    heartbeat(16_384, 4).await;
    let fits = placement::placement_preview(meta.as_ref(), &ctx, 8_192, 2)
        .await
        .expect("placement_preview");
    assert!(fits, "a 8 GiB session fits a 16 GiB survivor");

    // CPU dimension binds independently: plenty of RAM, but a 32-vCPU
    // ask against a 4-core × 4.0 = 16-vCPU budget → no fit.
    let fits = placement::placement_preview(meta.as_ref(), &ctx, 8_192, 32)
        .await
        .expect("placement_preview");
    assert!(
        !fits,
        "CPU budget binds before RAM — a 32-vCPU ask exceeds the 16-vCPU host budget"
    );

    // An UNMEASURED survivor (allocatable 0, no reported cores) keeps the
    // soft-fits posture reserve_placement takes for brand-new / dev hosts.
    heartbeat(0, 0).await;
    let fits = placement::placement_preview(meta.as_ref(), &ctx, 8_192, 32)
        .await
        .expect("placement_preview");
    assert!(fits, "an unmeasured survivor soft-fits any budget");
}
