//! Host cordon, lease, and binding-strike tests against Postgres.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use engram_core::types::BindingDisposition;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

/// ADR 0116: the lazily-passed cold-boot materialization, pre-resolved
/// for tests (production passes `materialize_cold_boot` un-awaited).
use engram_core::traits::{HarnessDial, HostClient, MetadataStore};

use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxError, SandboxId, SessionId, SnapshotId};
use engram_storage_local::LocalBlobStorage;
use parking_lot::Mutex;

struct TestRig {
    meta: Arc<dyn MetadataStore>,
    /// URL of this rig's private database, for tests that need a raw pool.
    _blob_dir: tempfile::TempDir,
}

async fn rig() -> Option<TestRig> {
    // ADR 0047: placement reads the GLOBAL hosts table, so tests in this
    // binary cannot share a database — a sibling test's fresh host row
    // would be a legal pick. ADR 0099 H1: each rig clones its own
    // database from the migrated template.
    let db = engram_testkit::pg::fresh_db().await?;
    let store = db.store;

    let blob_dir = tempfile::tempdir().expect("tempdir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> =
        Arc::new(LocalBlobStorage::new(blob_dir.path().to_path_buf()));
    let _ = blob;

    let pg = Arc::new(store);
    Some(TestRig {
        meta: pg as Arc<dyn MetadataStore>,
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
            peer_hints: Vec::new(),
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
    async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }
    async fn bind_session(
        &self,
        _session_id: SessionId,
        _sandbox_id: SandboxId,
        _binding_epoch: u64,
    ) -> Result<(), engram_core::SandboxError> {
        Ok(())
    }
    async fn unbind_session(&self, _session_id: SessionId) {}
    async fn send_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
        _text: String,
        _mode: Option<String>,
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
        sandbox_bundles: Vec::new(),
        cordoned: false,
        cordon_owner: None,
        cordon_reason: None,
        retire_requested_at: None,
        retired_at: None,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
        capabilities: engram_core::types::host::HostCapabilities::default(),
        lease_expires_at: None,
        lease_state: Default::default(),
        lease_epoch: 0,
    })
    .await
    .expect("upsert_host");
}

/// Seed a host row with an explicit `status` + `last_heartbeat_at` (and a
/// NULL lease) so a test can stage candidates for the dead-host detector's
/// `list_lease_expired_hosts` query.
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
        sandbox_bundles: Vec::new(),
        cordoned: false,
        cordon_owner: None,
        cordon_reason: None,
        retire_requested_at: None,
        retired_at: None,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
        capabilities: engram_core::types::host::HostCapabilities::default(),
        lease_expires_at: None,
        lease_state: Default::default(),
        lease_epoch: 0,
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
    // create_session lands at Pending; bind via the production fused
    // path (0108 forbids a bound Pending row), then drive to Active.
    meta.transition_session_created(session_id, sandbox_id)
        .await
        .expect("Pending → Created (fused bind)");
    meta.transition_session(session_id, SessionState::Active, BindingDisposition::Retain)
        .await
        .expect("Created → Active");
    session_id
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

/// ADR 0047: the durable coordinator cordon. `set_host_cordon` writes
/// the PG bit; `placement::pick_for_session` (reading host rows) must
/// skip the host — from ANY replica (a second registry over the same
/// store sees the same cordon), and the cordon survives heartbeats
/// (`touch_host_heartbeat` never writes the bit).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn durable_cordon_excludes_host_from_placement_on_every_replica() {
    use engram_coordinator::placement::{self, ScheduleContext};
    use engram_coordinator::HostRegistry;
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
        prefer_bundles: &[],
    };
    let (first_pick, _) =
        placement::pick_for_session(meta.as_ref(), &registry, &ctx, chrono::Utc::now())
            .await
            .expect("pick succeeds");
    assert!(first_pick == cordoned || first_pick == healthy);

    // Cordon — every replica's picker MUST avoid it.
    meta.set_host_cordon(
        cordoned,
        Some(engram_core::types::host::CordonOwner::Admin),
        None,
    )
    .await
    .expect("cordon a rowed host");
    for reg in [&registry, &replica_b] {
        for _ in 0..10 {
            let (picked, _) =
                placement::pick_for_session(meta.as_ref(), reg, &ctx, chrono::Utc::now())
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
            sandbox_bundles: Vec::new(),
            total_vcpus: 8,
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
            lease_renew_until: None,
        },
    )
    .await
    .expect("heartbeat");
    let (picked, _) =
        placement::pick_for_session(meta.as_ref(), &registry, &ctx, chrono::Utc::now())
            .await
            .expect("pick succeeds");
    assert_eq!(picked, healthy, "heartbeat must not clear the cordon");

    // Uncordon — the previously-cordoned host is eligible again. Use
    // exclude_host to force the pick deterministically.
    meta.set_host_cordon(cordoned, None, None)
        .await
        .expect("uncordon");
    let exclude_healthy_ctx = ScheduleContext {
        exclude_host: Some(healthy),
        ..ctx.clone()
    };
    let (picked, _) = placement::pick_for_session(
        meta.as_ref(),
        &registry,
        &exclude_healthy_ctx,
        chrono::Utc::now(),
    )
    .await
    .expect("post-uncordon pick must succeed when healthy host is excluded");
    assert_eq!(
        picked, cordoned,
        "after uncordon, the picker must return the previously-cordoned host"
    );

    // Unknown host id → NotFound (admin endpoint maps to 404).
    assert!(matches!(
        meta.set_host_cordon(
            HostId::new(),
            Some(engram_core::types::host::CordonOwner::Admin),
            None
        )
        .await,
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
        sandbox_bundles: Vec::new(),
        cordoned: false,
        cordon_owner: None,
        cordon_reason: None,
        retire_requested_at: None,
        retired_at: None,
        total_vcpus: 0,
        wire_version: 0,
        stages_images: false,
        capabilities: engram_core::types::host::HostCapabilities::default(),
        lease_expires_at: None,
        lease_state: Default::default(),
        lease_epoch: 0,
    })
    .await
    .expect("seed host row");
}

/// ADR 0044 K3 amendment, kept under ADR 0116 A-D4: the dead-host
/// detector must NOT strike out a `draining` host — it's operator-managed
/// (mid image-roll, where K2 reattach keeps its VMs alive across the
/// pod-swap heartbeat gap, or mid node-removal). `list_lease_expired_hosts`
/// returns only `ready` rows, and a NULL lease on a ready row reads as
/// expired (no lease ⇒ no shield).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn lease_expiry_list_excludes_draining_hosts() {
    use engram_core::types::HostStatus;
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();

    // Both seeded rows carry a NULL lease (`seed_host_with` writes
    // `lease_expires_at: None`) ⇒ both read as expired; only status
    // separates them.
    let now = Utc::now();
    let unleased_ready = HostId::new();
    let unleased_draining = HostId::new();
    seed_host_with(&meta, unleased_ready, "nl-ready", HostStatus::Ready, now).await;
    seed_host_with(
        &meta,
        unleased_draining,
        "nl-drain",
        HostStatus::Draining,
        now,
    )
    .await;
    let leased_ready = HostId::new();
    seed_host_with(&meta, leased_ready, "leased-ready", HostStatus::Ready, now).await;
    assert!(meta
        .renew_host_lease(leased_ready, now + chrono::Duration::seconds(45))
        .await
        .expect("renew_host_lease"));

    let expired_ids: Vec<HostId> = meta
        .list_lease_expired_hosts()
        .await
        .expect("list_lease_expired_hosts")
        .into_iter()
        .map(|h| h.id)
        .collect();

    assert!(
        expired_ids.contains(&unleased_ready),
        "a NULL-lease READY host is a candidate immediately"
    );
    assert!(
        !expired_ids.contains(&unleased_draining),
        "a DRAINING host is operator-managed — must be excluded"
    );
    assert!(!expired_ids.contains(&leased_ready), "a live lease shields");
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

/// Deletion requires retirement even after the last bound session leaves.
/// A second deletion succeeds when the row is already absent.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn delete_host_refuses_unretired_then_idempotent() {
    use engram_core::types::host::DeleteHostOutcome;
    let Some(rig) = rig().await else { return };
    let meta = rig.meta.clone();

    let host = HostId::new();
    let sid = seed_active_session(&meta, host, SandboxId::new()).await;

    // A ready host cannot be deleted.
    match meta.delete_host(host).await.expect("delete_host") {
        DeleteHostOutcome::NotRetired(status) => {
            assert_eq!(status, engram_core::types::host::HostStatus::Ready)
        }
        other => panic!("expected NotRetired(Ready) while a session is Active, got {other:?}"),
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
    meta.transition_session(sid, SessionState::Idle, BindingDisposition::Detach)
        .await
        .expect("Active → Idle");

    meta.request_host_retirement(
        host,
        engram_core::types::host::CordonOwner::Admin,
        "test",
        chrono::DateTime::UNIX_EPOCH,
    )
    .await
    .unwrap();
    assert_eq!(
        meta.grant_host_retirement(host, chrono::DateTime::UNIX_EPOCH)
            .await
            .unwrap(),
        engram_core::types::host::RetirementGrant::Granted
    );

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
