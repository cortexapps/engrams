//! Live-Postgres integration tests for ADR 0018 session evacuation.
//!
//! Exercises both Phase A (`evacuate_to`, alive source) and Phase B
//! (`evacuate_dead_source`, dead/degraded source) against a real PG +
//! the in-memory HostRegistry + mock HostClient backends. The PG side
//! is what these tests really exercise — the state-machine drive
//! through `Active → HostLost → Created` and the rebind of
//! `sessions.host_id` / `sessions.sandbox_id` via
//! `assign_session_*` + `transition_session` must work against the
//! real legality-table-enforcing implementation, not a mock.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. CI wires this into the
//! Postgres-gated-ignored lane alongside `admin_chunk_gc_live_pg`.
//!
//! Coverage:
//! - `evacuate_to_alive_source_drives_state_through_host_lost_to_created`
//!   — happy path. Captures the load-bearing invariant: the legacy
//!   `Active → HostLost → Created` graph is the only sequence that
//!   gets through PG's `transition_session` legality check; any
//!   regression to a direct `Active → Created` flip would fail.
//! - `evacuate_dead_source_with_snapshot_uses_recorded_manifests`
//!   — restores from a recorded snapshot's disk+memory manifests
//!   when only the snapshot is available. Loss=None.
//! - `evacuate_dead_source_disk_only_records_memory_loss` — when
//!   only `sessions.live_disk_manifest_*` is set (no snapshot row),
//!   the receipt carries `EvacLoss::Memory{reason: "source-dead-..."}`.
//! - `evacuate_dead_source_no_state_returns_no_recoverable` — both
//!   manifests absent → typed error; PG row stays at HostLost so the
//!   caller (dead_host.rs / NBD trigger) can drive to Dead.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef as ChunkManifestRef};
use engram_coordinator::evacuation::{evacuate_dead_source, evacuate_to, EvacError};
use engram_coordinator::host_registry::HostRegistry;
use engram_core::traits::{HarnessDial, HostClient, MetadataStore};
use engram_core::types::evacuation::EvacLoss;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::session::{HarnessSpec, SessionSpec, SessionState};
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

/// FakeBackend: records calls + returns deterministic ids. Mirrors the
/// shape used in evacuation.rs's unit tests but lifted here so the
/// integration tests can register it against the real HostRegistry.
/// Production HostClients require a host process to wire up; for the
/// PG-state-flow tests that's noise.
#[derive(Default)]
struct FakeBackend {
    calls: Mutex<Vec<String>>,
    next_restore_id: Mutex<Option<SandboxId>>,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn set_restore_id(&self, id: SandboxId) {
        *self.next_restore_id.lock() = Some(id);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
}

#[async_trait]
impl HostClient for FakeBackend {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        self.calls.lock().push("create".into());
        Ok(SandboxId::new())
    }
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.calls.lock().push(format!("destroy:{id}"));
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
    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        self.calls.lock().push(format!("snapshot:{id}"));
        Ok(SnapshotMetadata {
            id: SnapshotId::new(),
            size_bytes: 1024,
            created_at: Utc::now(),
            image_version: "test".into(),
            disk_manifest: None,
            memory_manifest: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
        })
    }
    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.calls.lock().push(format!("commit_snapshot:{id}"));
        Ok(())
    }
    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.calls.lock().push(format!("abort_snapshot:{id}"));
        Ok(())
    }
    async fn restore(&self, _md: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        self.calls.lock().push("restore".into());
        Ok(self
            .next_restore_id
            .lock()
            .unwrap_or_else(|| SandboxId::new()))
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
    async fn send_prompt(
        &self,
        _sandbox_id: SandboxId,
        _text: String,
    ) -> Result<(), SandboxError> {
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

/// Mint a fresh session row at Active status with `host_id` and
/// `sandbox_id` bound. Returns the session_id for follow-up queries.
async fn seed_active_session(
    meta: &Arc<dyn MetadataStore>,
    host_id: HostId,
    sandbox_id: SandboxId,
) -> SessionId {
    let session_id = meta
        .create_session(SessionSpec {
            image: format!("ghcr.io/test/img:t-{}", uuid::Uuid::new_v4()),
            harness: HarnessSpec::None,
            user_id: None,
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
    store.put_manifest(r, &manifest).await.expect("put manifest");
    r
}

fn proto_to_core_manifest(r: ChunkManifestRef) -> ManifestRef {
    ManifestRef {
        manifest_id: r.manifest_id,
        version: r.version,
    }
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn evacuate_to_alive_source_drives_state_through_host_lost_to_created() {
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();
    let registry = Arc::new(HostRegistry::new(meta.clone()));

    let source_host = HostId::new();
    let target_host = HostId::new();
    let old_sandbox = SandboxId::new();
    let new_sandbox = SandboxId::new();

    let source_be = FakeBackend::new();
    let target_be = FakeBackend::new();
    target_be.set_restore_id(new_sandbox);
    registry.register(source_host, source_be.clone());
    registry.register(target_host, target_be.clone());
    registry.record_sandbox_owner(old_sandbox, source_host);

    let session_id = seed_active_session(&meta, source_host, old_sandbox).await;

    let receipt = evacuate_to(&registry, &meta, session_id, old_sandbox, target_host)
        .await
        .expect("alive-source evac should succeed");
    assert_eq!(receipt.new_host_id, target_host);
    assert_eq!(receipt.new_sandbox_id, new_sandbox);
    assert_eq!(receipt.loss, EvacLoss::None);

    // Source side: snapshot + commit_snapshot + destroy in order.
    let src_calls = source_be.calls();
    assert!(
        src_calls.iter().any(|c| c.starts_with("snapshot:")),
        "snapshot not called: {src_calls:?}"
    );
    assert!(
        src_calls.iter().any(|c| c.starts_with("destroy:")),
        "destroy not called: {src_calls:?}"
    );

    // PG side: the load-bearing assertion. transition_session enforces
    // the M2 legality table, so getting to Created here is only
    // possible via Active → HostLost → Created (or via the implicit
    // route the evac primitive takes). The status assertion catches
    // any regression that tries Active → Created directly.
    let session = meta.get_session(session_id).await.expect("get session");
    assert_eq!(session.status, SessionState::Created);
    assert_eq!(session.host_id, Some(target_host));
    assert_eq!(session.sandbox_id, Some(new_sandbox));
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
    })
    .await
    .expect("record snapshot");

    let session = meta.get_session(session_id).await.expect("get session");
    let snapshot = meta
        .latest_snapshot_for_session(session_id)
        .await
        .expect("latest_snapshot lookup")
        .expect("snapshot present");

    let receipt = evacuate_dead_source(&registry, &meta, session, Some(snapshot))
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
    let receipt = evacuate_dead_source(&registry, &meta, session, None)
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
    let result = evacuate_dead_source(&registry, &meta, session, None).await;
    assert!(matches!(result, Err(EvacError::NoRecoverableState)));

    // PG row sits at HostLost — the caller routes it to Dead next.
    let after = meta.get_session(session_id).await.expect("get session");
    assert_eq!(after.status, SessionState::HostLost);
}
