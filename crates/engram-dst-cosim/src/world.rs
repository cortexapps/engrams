//! The co-simulated world: ONE shared `SimClock` + `SimMetadataStore`, a
//! real coordinator replica, and a real [`CosimHost`], wired across the
//! boundary bridge (ADR 0098 R-CoSim, rung 1).
//!
//! The single shared clock is the crux: `engram-dst` and `engram-dst-host`
//! each build their own paused-clock world; here BOTH the coordinator's
//! `Services` and the host's `CosimHost` read the same `SimClock`, so a time
//! advance moves both sides together and the interleaving is deterministic.

use std::sync::Arc;

use engram_coordinator::state::SharedState;
use engram_coordinator::{AppState, CoordinatorConfig, HostRegistry, Services};
use engram_core::traits::{Clock, Entropy as _, HostClient, MetadataStore};
use engram_core::types::host::{
    HostCapacity, HostHeartbeat, HostRecord, HostStatus, HostUtilization,
};
use engram_core::{HostId, SessionId};
use engram_sim::{SimClock, SimEntropy, SimMetadataStore};
use uuid::Uuid;

use crate::bridge::{CosimCoordControlPlane, CosimHostClient};
use crate::host::{CosimHost, HostView, SharedHost};

/// The one image every co-sim session boots.
pub const COSIM_IMAGE: &str = "cosim:warm-image";

/// The co-simulated world.
pub struct CosimWorld {
    pub clock: Arc<SimClock>,
    pub entropy: Arc<SimEntropy>,
    pub meta: Arc<SimMetadataStore>,
    pub host_id: HostId,
    /// The shared, lock-guarded host both bridge directions mutate.
    pub host: SharedHost,
    /// The reconcile-visible slice of host state (for building reconcile
    /// backends + oracle reads).
    pub view: Arc<HostView>,
    /// The single coordinator replica (stateless over the shared store).
    /// `SharedState` is already `Arc<AppState>`.
    pub state: SharedState,
    /// The host-agent's control-plane bridge (calls the real coordinator
    /// handler cores).
    pub coord_plane: Arc<CosimCoordControlPlane>,
}

impl CosimWorld {
    pub async fn new(seed: u64) -> Self {
        let clock = SimClock::new();
        let entropy = Arc::new(SimEntropy::seeded(seed));
        let meta = SimMetadataStore::new(clock.clone(), Arc::new(SimEntropy::seeded(seed ^ 0xE)));
        let host_id = HostId::from(Uuid::from_u128(0x0A57_0000));

        let host: SharedHost = Arc::new(tokio::sync::Mutex::new(CosimHost::new(
            host_id,
            clock.clone(),
            entropy.clone(),
        )));
        let view = host.lock().await.view();

        let state = build_replica(&meta, &clock, &entropy, host_id, host.clone());
        let coord_plane = Arc::new(CosimCoordControlPlane::new(state.clone()));

        let world = Self {
            clock,
            entropy,
            meta,
            host_id,
            host,
            view,
            state,
            coord_plane,
        };
        world.seed_enabled_image(COSIM_IMAGE).await;
        world.heartbeat().await;
        world
    }

    /// A new [`CosimReconcileBackend`](crate::host::CosimReconcileBackend)
    /// over this world's host — `honor_capture_signal` is the #570
    /// red/green knob (`false` replays the pre-fix reconcile).
    pub fn reconcile_backend(
        &self,
        honor_capture_signal: bool,
    ) -> crate::host::CosimReconcileBackend {
        crate::host::CosimReconcileBackend::new(
            self.host.clone(),
            self.view.clone(),
            honor_capture_signal,
        )
    }

    /// Register + heartbeat the host into the coordinator meta so the
    /// scheduler can place sessions on it.
    pub async fn heartbeat(&self) {
        let _ = self
            .meta
            .upsert_host(host_record(self.host_id, &self.clock))
            .await;
        let _ = self
            .meta
            .touch_host_heartbeat(self.host_id, host_heartbeat())
            .await;
    }

    /// Seed an enabled image (+ its base snapshot row) so the boot
    /// pipeline's prepare leg resolves it exactly like production.
    async fn seed_enabled_image(&self, uri: &str) {
        let now = self.clock.now_utc();
        let base_snapshot_id = engram_core::SnapshotId::from(self.entropy.uuid());
        let snapshot: engram_core::types::snapshot::SnapshotRecord =
            serde_json::from_value(serde_json::json!({
                "id": base_snapshot_id,
                "session_id": null,
                "host_id": null,
                "image_version": uri,
                "size_bytes": 0,
                "created_at": now,
                "last_accessed_at": now,
                "recoverable": true,
            }))
            .expect("minimal base snapshot row");
        let image = engram_core::types::EnabledImage {
            id: self.entropy.uuid(),
            image_uri: uri.to_string(),
            image_config: toml::from_str(r#"name = "cosim""#).expect("cosim image config"),
            oci_defaults: Default::default(),
            manifest_digest: "sha256:cosim".into(),
            disk_manifest: None,
            base_snapshot_id: Some(base_snapshot_id),
            base_snapshot_disk_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: self.entropy.uuid(),
                version: 1,
            }),
            base_snapshot_memory_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: self.entropy.uuid(),
                version: 1,
            }),
            last_refreshed_at: now,
            created_at: now,
            updated_at: None,
            soft_deleted_at: None,
        };
        self.meta
            .record_snapshot(snapshot)
            .await
            .expect("seed base snapshot");
        self.meta
            .upsert_enabled_image(image)
            .await
            .expect("seed enabled image");
    }

    /// The shared clock's current wall time.
    pub fn clock_now(&self) -> chrono::DateTime<chrono::Utc> {
        self.clock.now_utc()
    }

    /// The newest recoverable snapshot cursor for a session (the value a
    /// resume would restore to), or `None` if none exists.
    pub fn newest_recoverable_cursor(&self, session_id: SessionId) -> Option<i64> {
        self.meta.with_db(|db| {
            db.snapshots
                .values()
                .filter(|s| s.session_id == Some(session_id) && s.recoverable)
                .filter_map(|s| s.events_cursor)
                .max()
        })
    }
}

fn build_replica(
    meta: &Arc<SimMetadataStore>,
    clock: &Arc<SimClock>,
    entropy: &Arc<SimEntropy>,
    host_id: HostId,
    host: SharedHost,
) -> SharedState {
    let registry = Arc::new(HostRegistry::new(meta.clone() as Arc<dyn MetadataStore>));
    registry.register(
        host_id,
        Arc::new(CosimHostClient::new(host_id, host)) as Arc<dyn HostClient>,
    );
    let blob_dir = std::env::temp_dir().join("engram-dst-cosim-blobs");
    let services = Services {
        meta: meta.clone(),
        cloud: Arc::new(engram_cloud_mock::MockCloud::new()),
        host: registry.clone() as Arc<dyn HostClient>,
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0u8; 32], "cosim:v1",
        )),
        oci: Arc::new(engram_oci::OciClient::new(Arc::new(
            engram_oci::AnonymousResolver,
        ))),
        auth_resolver: Arc::new(engram_oci::AnonymousResolver),
        blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
            blob_dir.clone(),
        )),
        chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
            engram_storage_local::LocalBlobStorage::new(blob_dir),
        )),
        materialize_dir: None,
        clock: clock.clone(),
        entropy: entropy.clone(),
    };
    // `new_with_registry` returns `AppState`; wrap once into
    // `SharedState = Arc<AppState>`.
    Arc::new(AppState::new_with_registry(
        CoordinatorConfig::default(),
        services,
        registry,
    ))
}

fn host_utilization() -> HostUtilization {
    serde_json::from_value(serde_json::json!({ "allocatable_mib": 24_576 }))
        .expect("utilization from defaults")
}

fn host_capacity() -> HostCapacity {
    HostCapacity {
        total_gb: 100,
        used_gb: 0,
        total_mib: 32_768,
        used_mib: 0,
        running_sandboxes: 0,
    }
}

fn host_heartbeat() -> HostHeartbeat {
    HostHeartbeat {
        status: HostStatus::Ready,
        capacity: host_capacity(),
        utilization: host_utilization(),
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
    }
}

fn host_record(id: HostId, clock: &Arc<SimClock>) -> HostRecord {
    HostRecord {
        id,
        hostname: format!("cosim-{id}"),
        cloud_metadata: Default::default(),
        capacity: host_capacity(),
        utilization: host_utilization(),
        status: HostStatus::Ready,
        last_heartbeat_at: clock.now_utc(),
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
