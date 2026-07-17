//! The simulated world: replicas, hosts, and the world-truth the
//! invariant checkers compare against.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use engram_coordinator::{AppState, CoordinatorConfig, HostRegistry, Services};
use engram_core::traits::{Entropy as _, HostClient, SessionFence};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxProbe, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxError, SandboxId, SessionId};
use engram_sim::{SimClock, SimEntropy, SimMetadataStore};
use parking_lot::Mutex;

/// World-side truth for one simulated host. The coordinator's beliefs
/// (host rows, session bindings) are compared against THIS by the
/// invariant checkers — the whole point of keeping it outside SimMeta.
#[derive(Debug, Default)]
pub struct SimHostState {
    pub up: bool,
    /// Asymmetric partition: the host is UP and serves RPCs, but its
    /// heartbeats never land (the dead-host detector's hardest case —
    /// the issue-#231 probe exists exactly for this).
    pub heartbeats_partitioned: bool,
    /// The inverse: heartbeats land but RPCs fail (a half-open
    /// connection / one-way network fault).
    pub rpc_partitioned: bool,
    /// sandbox -> owning session (as told to us via create's spec).
    pub sandboxes: BTreeMap<SandboxId, Option<SessionId>>,
}

#[derive(Debug, Default)]
pub struct SimHostWorld {
    pub hosts: Mutex<BTreeMap<HostId, SimHostState>>,
}

impl SimHostWorld {
    fn with_host<R>(
        &self,
        id: HostId,
        f: impl FnOnce(&mut SimHostState) -> Result<R, SandboxError>,
    ) -> Result<R, SandboxError> {
        let mut hosts = self.hosts.lock();
        let host = hosts.get_mut(&id).ok_or(SandboxError::HostLost)?;
        if !host.up {
            // The same typed retryable error the real transport surfaces
            // when a host stops answering.
            return Err(SandboxError::Unavailable(format!("sim: host {id} is down")));
        }
        f(host)
    }
}

/// A per-host `HostClient` over the shared world. Registered into each
/// replica's `HostRegistry` under its host id, exactly like a real
/// remote host's client.
#[derive(Debug)]
pub struct SimHostClient {
    pub host_id: HostId,
    pub world: Arc<SimHostWorld>,
    pub entropy: Arc<SimEntropy>,
}

#[async_trait]
impl HostClient for SimHostClient {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        let id = SandboxId::from(self.entropy.uuid());
        self.world.with_host(self.host_id, |h| {
            // Ownership is learned at bind_session time (the spec is a
            // template, not a binding — see sandbox.rs's type docs).
            h.sandboxes.insert(id, None);
            Ok(id)
        })
    }

    async fn destroy(&self, id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        self.world.with_host(self.host_id, |h| {
            h.sandboxes.remove(&id);
            Ok(())
        })
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.world
            .with_host(self.host_id, |h| Ok(h.sandboxes.keys().copied().collect()))
    }

    async fn probe_sandbox(&self, id: SandboxId) -> Result<SandboxProbe, SandboxError> {
        self.world.with_host(self.host_id, |h| {
            let known = h.sandboxes.contains_key(&id);
            Ok(SandboxProbe {
                known_to_backend: known,
                process_alive: known,
                control_alive: Some(known),
            })
        })
    }

    async fn exec_stream(
        &self,
        _id: SandboxId,
        _cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        // The D5 workload never execs; guest behavior is out of scope
        // (ADR 0098 non-goal). Fail loudly if a driver starts doing it.
        Err(SandboxError::Unsupported(
            "sim: exec is not modeled (ADR 0098 non-goal)".into(),
        ))
    }

    async fn snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let entropy = self.entropy.clone();
        self.world.with_host(self.host_id, |h| {
            if !h.sandboxes.contains_key(&id) {
                return Err(SandboxError::NotFound);
            }
            // Round-trip through serde: every Option field is
            // `#[serde(default)]`, so an id-only JSON object IS the
            // canonical minimal metadata — no hand-listing 12 fields.
            let meta: SnapshotMetadata = serde_json::from_value(serde_json::json!({
                "id": engram_core::SnapshotId::from(entropy.uuid()),
                "size_bytes": 0,
                "created_at": chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
                "image_version": "sim",
            }))
            .expect("minimal snapshot metadata");
            Ok(meta)
        })
    }

    async fn restore(
        &self,
        _metadata: SnapshotMetadata,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        let id = SandboxId::from(self.entropy.uuid());
        self.world.with_host(self.host_id, |h| {
            h.sandboxes.insert(id, None);
            Ok(id)
        })
    }

    async fn start_agent(
        &self,
        id: SandboxId,
        _agent: AgentSpec,
        _policy: SessionEgressPolicy,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.world.with_host(self.host_id, |h| {
            if h.sandboxes.contains_key(&id) {
                Ok(())
            } else {
                Err(SandboxError::NotFound)
            }
        })
    }

    /// The boot pipeline's actual restore leg (fresh create = restore
    /// from the image's base snapshot). Allocates a sandbox on this
    /// host — the default impl errors, which left every sim boot
    /// retrying to Failed.
    async fn restore_base_for_session(
        &self,
        _metadata: SnapshotMetadata,
        _session_env: std::collections::HashMap<String, String>,
        _selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        let id = SandboxId::from(self.entropy.uuid());
        self.world.with_host(self.host_id, |h| {
            h.sandboxes.insert(id, None);
            Ok(id)
        })
    }

    async fn apply_egress_policy(&self, _policy: SessionEgressPolicy) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }

    async fn bind_session(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        _binding_epoch: u64,
    ) {
        let _ = self.world.with_host(self.host_id, |h| {
            if let Some(owner) = h.sandboxes.get_mut(&sandbox_id) {
                *owner = Some(session_id);
            }
            Ok(())
        });
    }

    async fn unbind_session(&self, session_id: SessionId) {
        let _ = self.world.with_host(self.host_id, |h| {
            for owner in h.sandboxes.values_mut() {
                if *owner == Some(session_id) {
                    *owner = None;
                }
            }
            Ok(())
        });
    }

    async fn send_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
        _text: String,
    ) -> Result<(), SandboxError> {
        self.world.with_host(self.host_id, |_| Ok(()))
    }
}

/// One coordinator replica: a full real `AppState` over the SHARED
/// SimMeta. Crash = drop it; restart = rebuild (ADR 0047 statelessness,
/// exercised directly).
pub struct Replica {
    pub state: Option<engram_coordinator::state::SharedState>,
    /// This replica's view of wall-clock time — a SimClock over the
    /// same paused tokio base, with its own (fault-mutable) skew.
    pub clock: Arc<SimClock>,
}

pub struct SimWorld {
    pub clock: Arc<SimClock>,
    pub entropy: Arc<SimEntropy>,
    pub meta: Arc<SimMetadataStore>,
    pub host_world: Arc<SimHostWorld>,
    pub host_ids: Vec<HostId>,
    pub replicas: Vec<Replica>,
}

impl SimWorld {
    pub fn new(seed: u64, replicas: usize, hosts: usize) -> Self {
        let clock = SimClock::new();
        let entropy = Arc::new(SimEntropy::seeded(seed));
        let meta = SimMetadataStore::new(clock.clone(), Arc::new(SimEntropy::seeded(seed ^ 0xE)));
        let host_world = Arc::new(SimHostWorld::default());
        let host_ids: Vec<HostId> = (0..hosts)
            .map(|i| {
                // Deterministic, readable host ids.
                HostId::from(::uuid::Uuid::from_u128(0x0D57_0000 + i as u128))
            })
            .collect();
        {
            let mut hw = host_world.hosts.lock();
            for id in &host_ids {
                hw.insert(
                    *id,
                    SimHostState {
                        up: true,
                        ..SimHostState::default()
                    },
                );
            }
        }
        let mut world = Self {
            clock,
            entropy,
            meta,
            host_world,
            host_ids,
            replicas: Vec::new(),
        };
        for _ in 0..replicas {
            let clock = SimClock::new();
            let state = world.build_replica_with_clock(clock.clone());
            world.replicas.push(Replica {
                state: Some(state),
                clock,
            });
        }
        world
    }

    /// Build a fresh replica over the shared authority — also the
    /// restart path after a crash fault. The replica keeps its own
    /// clock (with any accumulated skew) across restarts — machines
    /// keep their clocks when processes die.
    pub fn build_replica_with_clock(
        &self,
        clock: Arc<SimClock>,
    ) -> engram_coordinator::state::SharedState {
        let registry = Arc::new(HostRegistry::new(
            self.meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        for id in &self.host_ids {
            registry.register(
                *id,
                Arc::new(SimHostClient {
                    host_id: *id,
                    world: self.host_world.clone(),
                    entropy: self.entropy.clone(),
                }) as Arc<dyn HostClient>,
            );
        }
        let blob_dir = std::env::temp_dir().join("engram-dst-blobs");
        let services = Services {
            meta: self.meta.clone(),
            cloud: Arc::new(engram_cloud_mock::MockCloud::new()),
            host: registry.clone() as Arc<dyn HostClient>,
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "sim:v1",
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
            clock,
            entropy: self.entropy.clone(),
        };
        Arc::new(AppState::new_with_registry(
            CoordinatorConfig::default(),
            services,
            registry,
        ))
    }
}

impl SimWorld {
    /// Seed an enabled image (+ its base snapshot row) so the boot
    /// pipeline's prepare leg resolves it exactly like production.
    pub fn seed_enabled_image(&self, uri: &str) {
        use engram_core::traits::{Clock as _, Entropy as _, MetadataStore as _};
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
            image_config: toml::from_str(r#"name = "sim""#).expect("sim image config"),
            oci_defaults: Default::default(),
            manifest_digest: "sha256:sim".into(),
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
        // Synchronous seeding on a fresh world — block_on is fine here
        // (no runtime nesting: called before the sim loop starts)... but
        // we ARE inside the test's runtime, so spawn-and-wait instead.
        let meta = self.meta.clone();
        futures_block(async move {
            meta.record_snapshot(snapshot)
                .await
                .expect("seed base snapshot");
            meta.upsert_enabled_image(image)
                .await
                .expect("seed enabled image");
        });
    }
}

/// Poll a future to completion on the CURRENT thread without a nested
/// runtime — valid because SimMeta never actually suspends.
fn futures_block<F: std::future::Future<Output = ()>>(f: F) {
    let mut f = Box::pin(f);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    for _ in 0..1024 {
        if let std::task::Poll::Ready(()) = f.as_mut().poll(&mut cx) {
            return;
        }
    }
    panic!("seed future did not complete synchronously");
}
