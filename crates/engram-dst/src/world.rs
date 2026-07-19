//! The simulated world: replicas, hosts, and the world-truth the
//! invariant checkers compare against.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use engram_coordinator::{AppState, CoordinatorConfig, HostRegistry, Services};
use engram_core::traits::{BlobStorage as _, Entropy as _, HostClient, SessionFence};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::manifest::ManifestRef;
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
    /// The inverse of the heartbeat partition: heartbeats land but RPC
    /// verbs STALL (ADR 0098 coverage-gap G1, the #743 op-wedge class).
    /// `Some(d)` = every RPC on this host sleeps `d` of virtual time
    /// before answering — above the verb's `op_deadline` this is the
    /// 03e6535e wedge (the within-step heartbeat keeps the op row fresh
    /// while the dispatch never returns; only the tokio-timer deadline
    /// unwedges it), below it a plain delay. Replaces the fail-fast
    /// `rpc_partitioned` flag that was toggled but never read.
    pub rpc_hang: Option<std::time::Duration>,
    /// sandbox -> owning session (as told to us via create's spec).
    pub sandboxes: BTreeMap<SandboxId, Option<SessionId>>,
}

/// A mutating host-verb's WORLD-side effect (ADR 0098 R2). Every
/// state-changing `SimHostClient` verb produces one of these instead of
/// touching `SimHostState.sandboxes` inline. In the default (inline)
/// mode the effect applies immediately — byte-for-byte the pre-R2 world,
/// so Calm seeds are unchanged. When the target host is in the deferred
/// set (a Chaos fault window), the effect is queued for a later
/// scheduler step to deliver / drop / duplicate / reorder — which is
/// what makes RPC loss/reorder/dup and a replica crash BETWEEN the store
/// commit (the ack the coordinator already got) and the world effect a
/// reachable state (the audit's finding #2/#4).
#[derive(Debug, Clone)]
pub enum Effect {
    /// `create` / `restore` / `restore_base_for_session`: a new sandbox
    /// appears on the host, ownership learned later at bind time.
    Create {
        sandbox: SandboxId,
    },
    Destroy {
        sandbox: SandboxId,
    },
    Bind {
        session: SessionId,
        sandbox: SandboxId,
    },
    Unbind {
        session: SessionId,
    },
}

#[derive(Debug, Clone)]
pub struct QueuedEffect {
    pub host: HostId,
    pub effect: Effect,
}

/// Deferred host-effects, keyed by a monotonic serial so delivery order
/// (and the seeded reorder fault) is deterministic. `deferred` is the set
/// of hosts whose verbs currently enqueue instead of applying inline.
#[derive(Debug, Default)]
pub struct EffectQueue {
    next_serial: u64,
    pending: BTreeMap<u64, QueuedEffect>,
    deferred: BTreeSet<HostId>,
}

#[derive(Debug, Default)]
pub struct SimHostWorld {
    pub hosts: Mutex<BTreeMap<HostId, SimHostState>>,
    pub effects: Mutex<EffectQueue>,
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

    /// The RPC-time liveness gate for a MUTATING verb: a down/unknown host
    /// fails exactly as `with_host` did (so a create against a dead host
    /// still surfaces `Unavailable`/`HostLost`), but the world mutation is
    /// separated out into an [`Effect`] recorded via [`record_effect`].
    fn require_up(&self, id: HostId) -> Result<(), SandboxError> {
        let hosts = self.hosts.lock();
        let host = hosts.get(&id).ok_or(SandboxError::HostLost)?;
        if !host.up {
            return Err(SandboxError::Unavailable(format!("sim: host {id} is down")));
        }
        Ok(())
    }

    /// Record a verb's world-effect. Inline unless the host is deferred.
    fn record_effect(&self, host: HostId, effect: Effect) {
        let deferred = {
            let mut q = self.effects.lock();
            if q.deferred.contains(&host) {
                let serial = q.next_serial;
                q.next_serial += 1;
                q.pending.insert(
                    serial,
                    QueuedEffect {
                        host,
                        effect: effect.clone(),
                    },
                );
                true
            } else {
                false
            }
        };
        if !deferred {
            self.apply_effect(host, &effect);
        }
    }

    /// Apply one effect to world truth. A down/absent host swallows it —
    /// the machine that would have held the sandbox is gone (a create
    /// effect never resurrects a restarted host's cleared VM set; a stale
    /// destroy is a harmless no-op).
    fn apply_effect(&self, host: HostId, effect: &Effect) {
        let mut hosts = self.hosts.lock();
        let Some(h) = hosts.get_mut(&host) else {
            return;
        };
        if !h.up {
            return;
        }
        match effect {
            Effect::Create { sandbox } => {
                h.sandboxes.entry(*sandbox).or_insert(None);
            }
            Effect::Destroy { sandbox } => {
                h.sandboxes.remove(sandbox);
            }
            Effect::Bind { session, sandbox } => {
                if let Some(owner) = h.sandboxes.get_mut(sandbox) {
                    *owner = Some(*session);
                }
            }
            Effect::Unbind { session } => {
                for owner in h.sandboxes.values_mut() {
                    if *owner == Some(*session) {
                        *owner = None;
                    }
                }
            }
        }
    }

    // --- Scheduler-driven queue control (ADR 0098 R2) -----------------

    /// Toggle a host's deferred window. While on, that host's mutating
    /// verbs enqueue; the committed-but-unapplied window opens.
    pub fn set_deferred(&self, host: HostId, on: bool) {
        let mut q = self.effects.lock();
        if on {
            q.deferred.insert(host);
        } else {
            q.deferred.remove(&host);
        }
    }

    pub fn clear_deferred(&self) {
        self.effects.lock().deferred.clear();
    }

    pub fn pending_serials(&self) -> Vec<u64> {
        self.effects.lock().pending.keys().copied().collect()
    }

    pub fn pending_len(&self) -> usize {
        self.effects.lock().pending.len()
    }

    /// Deliver every pending effect in serial (causal) order and clear the
    /// queue — the normal, fault-free delivery step and the quiescence
    /// flush.
    pub fn deliver_in_order(&self) {
        let drained: Vec<QueuedEffect> = {
            let mut q = self.effects.lock();
            std::mem::take(&mut q.pending).into_values().collect()
        };
        for qe in drained {
            self.apply_effect(qe.host, &qe.effect);
        }
    }

    /// Deliver pending effects in the caller-supplied (seeded-shuffled)
    /// serial order — the REORDER fault. Serials absent from `order` are
    /// dropped; the scheduler always passes a full permutation.
    pub fn deliver_shuffled(&self, order: &[u64]) {
        let pending: BTreeMap<u64, QueuedEffect> = {
            let mut q = self.effects.lock();
            std::mem::take(&mut q.pending)
        };
        for serial in order {
            if let Some(qe) = pending.get(serial) {
                self.apply_effect(qe.host, &qe.effect);
            }
        }
    }

    /// Drop one queued effect without applying it — the LOSS fault.
    pub fn drop_pending(&self, serial: u64) -> bool {
        self.effects.lock().pending.remove(&serial).is_some()
    }

    /// Apply one queued effect an EXTRA time while leaving it queued — the
    /// DUPLICATE fault. Our effects are map-keyed and hence idempotent, so
    /// this exercises the coordinator's tolerance of a re-delivered verb
    /// rather than corrupting world truth.
    pub fn duplicate_pending(&self, serial: u64) {
        let qe = self.effects.lock().pending.get(&serial).cloned();
        if let Some(qe) = qe {
            self.apply_effect(qe.host, &qe.effect);
        }
    }

    /// A crashed/restarted host severs its in-flight RPCs: pending effects
    /// targeting it die with the machine (never resurrected onto the
    /// cleared VM set).
    pub fn drop_host_pending(&self, host: HostId) {
        self.effects.lock().pending.retain(|_, qe| qe.host != host);
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

impl SimHostClient {
    /// The G1 RPC hang/delay fault: stall this verb by the host's
    /// configured `rpc_hang` before touching the world. The lock is
    /// released BEFORE the sleep; on the paused clock the sleep resolves
    /// deterministically — either auto-advance reaches it (a delay) or
    /// the caller's `op_deadline` timeout fires first and drops this
    /// future mid-sleep (the wedge). Never `future::pending()` — an
    /// un-timed caller would deadlock the run-step-to-completion
    /// scheduler.
    async fn maybe_hang(&self) {
        let hang = {
            let hosts = self.world.hosts.lock();
            hosts.get(&self.host_id).and_then(|h| h.rpc_hang)
        };
        if let Some(d) = hang {
            tokio::time::sleep(d).await;
        }
    }
}

#[async_trait]
impl HostClient for SimHostClient {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        self.maybe_hang().await;
        // Draw the id BEFORE the liveness gate so the entropy stream is
        // identical to the pre-effect-queue world even on a down-host
        // failure (Calm determinism).
        let id = SandboxId::from(self.entropy.uuid());
        self.world.require_up(self.host_id)?;
        // Ownership is learned at bind_session time (the spec is a
        // template, not a binding — see sandbox.rs's type docs).
        self.world
            .record_effect(self.host_id, Effect::Create { sandbox: id });
        Ok(id)
    }

    async fn destroy(&self, id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        self.maybe_hang().await;
        self.world.require_up(self.host_id)?;
        self.world
            .record_effect(self.host_id, Effect::Destroy { sandbox: id });
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.maybe_hang().await;
        self.world
            .with_host(self.host_id, |h| Ok(h.sandboxes.keys().copied().collect()))
    }

    async fn probe_sandbox(&self, id: SandboxId) -> Result<SandboxProbe, SandboxError> {
        self.maybe_hang().await;
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
        self.maybe_hang().await;
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
        self.maybe_hang().await;
        let entropy = self.entropy.clone();
        // Draw every id under the host lock (existence-gated, deterministic
        // ordering: snapshot, then disk, then memory manifest), then drop
        // the lock before touching blob storage.
        let (snapshot_id, disk_manifest, memory_manifest) =
            self.world.with_host(self.host_id, |h| {
                if !h.sandboxes.contains_key(&id) {
                    return Err(SandboxError::NotFound);
                }
                let snapshot_id = engram_core::SnapshotId::from(entropy.uuid());
                let disk_manifest = ManifestRef {
                    manifest_id: entropy.uuid(),
                    version: 1,
                };
                let memory_manifest = ManifestRef {
                    manifest_id: entropy.uuid(),
                    version: 1,
                };
                Ok((snapshot_id, disk_manifest, memory_manifest))
            })?;
        // FIDELITY (issue #790): a real evict capture uploads its chunked
        // disk+memory manifests AND the portable FC state.bin/sidecar.json
        // to blob storage BEFORE the coordinator records the row — so the
        // coordinator's honest `verify_snapshot_recoverable` (a real
        // `blob.head` on the manifest keys) and resume-time
        // `snapshot_artifacts_present` (state.bin/sidecar HEADs) both pass and
        // the row lands `recoverable = true`. The old manifest-less metadata
        // made every capture record `recoverable = false`, so an idle-evicted
        // session reached `Idle` with no recoverable durable copy and tripped
        // the snapshot-safety oracle. We back the refs with REAL blobs in the
        // SAME shared store the coordinator reads, mirroring engram-dst-host's
        // ledger discipline (model the artifact, never fake the flag).
        let blob = engram_storage_local::LocalBlobStorage::new(blob_dir());
        for key in [
            disk_manifest.storage_key(),
            memory_manifest.storage_key(),
            engram_chunk_store::snapshot_blob::state_blob_key(snapshot_id),
            engram_chunk_store::snapshot_blob::sidecar_blob_key(snapshot_id),
        ] {
            blob.put(&key, bytes::Bytes::from_static(b"sim"))
                .await
                .map_err(|e| SandboxError::Snapshot(format!("sim: blob put {key}: {e}")))?;
        }
        // Round-trip through serde: every other Option field is
        // `#[serde(default)]`, so this JSON IS the canonical metadata — no
        // hand-listing the remaining fields.
        let meta: SnapshotMetadata = serde_json::from_value(serde_json::json!({
            "id": snapshot_id,
            "size_bytes": 0,
            "created_at": chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
            "image_version": "sim",
            "disk_manifest": disk_manifest,
            "memory_manifest": memory_manifest,
        }))
        .expect("minimal snapshot metadata");
        Ok(meta)
    }

    async fn restore(
        &self,
        _metadata: SnapshotMetadata,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        self.maybe_hang().await;
        let id = SandboxId::from(self.entropy.uuid());
        self.world.require_up(self.host_id)?;
        self.world
            .record_effect(self.host_id, Effect::Create { sandbox: id });
        Ok(id)
    }

    async fn start_agent(
        &self,
        id: SandboxId,
        _agent: AgentSpec,
        _policy: SessionEgressPolicy,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.maybe_hang().await;
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
        self.maybe_hang().await;
        let id = SandboxId::from(self.entropy.uuid());
        self.world.require_up(self.host_id)?;
        self.world
            .record_effect(self.host_id, Effect::Create { sandbox: id });
        Ok(id)
    }

    async fn apply_egress_policy(&self, _policy: SessionEgressPolicy) -> Result<(), SandboxError> {
        self.maybe_hang().await;
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
        self.maybe_hang().await;
        if self.world.require_up(self.host_id).is_ok() {
            self.world.record_effect(
                self.host_id,
                Effect::Bind {
                    session: session_id,
                    sandbox: sandbox_id,
                },
            );
        }
    }

    async fn unbind_session(&self, session_id: SessionId) {
        self.maybe_hang().await;
        if self.world.require_up(self.host_id).is_ok() {
            self.world.record_effect(
                self.host_id,
                Effect::Unbind {
                    session: session_id,
                },
            );
        }
    }

    async fn send_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
        _text: String,
    ) -> Result<(), SandboxError> {
        self.maybe_hang().await;
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
        let blob_dir = blob_dir();
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
            // FIDELITY (R3 #722): the workload reserves `mem_budget_mib: 2048`
            // / `cpu_budget_vcpus: 2` at create (scheduler.rs), so the enabled
            // image MUST resolve to the SAME budget — in production
            // `reserve_and_persist_create` and `resolve_cold_boot_spec` both
            // derive from `ImageConfig::resolved_memory_mib`/`_vcpus`, so
            // create-reserve == resume-resolve by construction. The old
            // `name = "sim"`-only config left memory at DEFAULT_MEMORY_MIB
            // (4096) while create reserved 2048: a resume's cold-boot spec then
            // needed 4096 where the queued row recorded 2048, so the
            // queue-scanner precheck (2048, fits) and the resume verb's own gate
            // (4096, no fit) disagreed forever — an Idle↔Queued livelock the
            // faithful-host resume-capacity fix surfaced (seed 142). vCPUs
            // already default to DEFAULT_VCPUS (2) = the create budget; pin
            // memory to close the gap.
            image_config: toml::from_str(
                "name = \"sim\"\n[resources]\nsuggested_memory_mib = 2048\nsuggested_vcpus = 2\n",
            )
            .expect("sim image config"),
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

/// The shared on-disk blob store standing in for GCS. Every replica's
/// `Services.blob`/`chunk_store` and the `SimHostClient` capture path read
/// and write the SAME directory (as a real pod + host share one bucket), so
/// a manifest/state blob the host wrote at capture is HEAD-visible to the
/// coordinator's honest recoverability check. Keys are content-addressed by
/// seeded-unique ids, so presence is deterministic per seed regardless of
/// cross-seed residue in the dir.
fn blob_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("engram-dst-blobs")
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
