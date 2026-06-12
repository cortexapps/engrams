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
    let store = engram_postgres::PostgresStore::connect(&database_url)
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
    async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(vec![])
    }
    async fn exec_stream(
        &self,
        _id: SandboxId,
        _cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        unreachable!()
    }
    async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
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
        })
    }
    async fn commit_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn abort_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn restore(&self, _md: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
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
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn apply_egress_policy(
        &self,
        _policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _id: SandboxId) -> Option<String> {
        None
    }
    async fn bind_session(&self, _session_id: SessionId, _sandbox_id: SandboxId) {}
    async fn unbind_session(&self, _session_id: SessionId) {}
    async fn send_prompt(&self, _sandbox_id: SandboxId, _text: String) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn acquire_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn release_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
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
    })
    .await
    .expect("record snapshot");

    let session = meta.get_session(session_id).await.expect("get session");
    let snapshot = meta
        .latest_snapshot_for_session(session_id)
        .await
        .expect("latest_snapshot lookup")
        .expect("snapshot present");

    let receipt = evacuate_dead_source(&registry, &meta, session, Some(snapshot), None, None)
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
    let result = evacuate_dead_source(&registry, &meta, session, None, None, None).await;
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

/// HostRegistry::cordon flips `HostState.draining` such that
/// `pick_for_session` skips the host. Pins the load-bearing user-
/// visible promise of the cordon admin endpoint: cordoned hosts
/// cannot be picked as evac targets.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn host_registry_cordon_excludes_host_from_pick_for_session() {
    use engram_coordinator::host_registry::{HostRegistry, ScheduleContext};
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();
    let registry = Arc::new(HostRegistry::new(meta.clone()));

    let cordoned = HostId::new();
    let healthy = HostId::new();
    registry.register(cordoned, FakeBackend::new());
    registry.register(healthy, FakeBackend::new());

    // Both hosts available → either can be picked.
    let ctx = ScheduleContext {
        repo: "test/img",
        image_version: "v1",
        prefer_snapshot_id: None,
        memory_mib: None,
        required_image_digest: None,
        exclude_host: None,
        prefer_host: None,
    };
    let (first_pick, _) = registry.pick_for_session(&ctx).expect("pick succeeds");
    assert!(first_pick == cordoned || first_pick == healthy);

    // Cordon `cordoned` — picker MUST avoid it.
    assert!(registry.cordon(cordoned), "cordon a registered host");
    for _ in 0..20 {
        let (picked, _) = registry.pick_for_session(&ctx).expect("pick succeeds");
        assert_eq!(
            picked, healthy,
            "cordoned host must never be picked (got {picked} after cordon)"
        );
    }

    // Uncordon — the previously-cordoned host is now eligible. Use
    // exclude_host to filter the other healthy host out, then verify
    // the picker returns the (now-uncordoned) host. This avoids
    // depending on DashMap's iteration order, which is stable but
    // hash-dependent — the test would be flaky if we relied on it.
    assert!(registry.uncordon(cordoned), "uncordon known host");
    let exclude_healthy_ctx = ScheduleContext {
        repo: "test/img",
        image_version: "v1",
        prefer_snapshot_id: None,
        memory_mib: None,
        required_image_digest: None,
        exclude_host: Some(healthy),
        prefer_host: None,
    };
    let (picked, _) = registry
        .pick_for_session(&exclude_healthy_ctx)
        .expect("post-uncordon pick must succeed when healthy host is excluded");
    assert_eq!(
        picked, cordoned,
        "after uncordon, the picker must return the previously-cordoned host"
    );

    // Unknown host id → false (admin endpoint maps to 404).
    assert!(
        !registry.cordon(HostId::new()),
        "unknown host returns false"
    );
    assert!(
        !registry.uncordon(HostId::new()),
        "unknown host returns false"
    );
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
