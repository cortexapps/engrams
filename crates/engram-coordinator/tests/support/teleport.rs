use super::coordinator;
use async_trait::async_trait;
use coordinator::state::SharedState;
use coordinator::{AppState, CoordinatorConfig, HostRegistry, Services};
use engram_core::traits::{Clock, Entropy, HarnessDial, HostClient, MetadataStore, SessionFence};
use engram_core::types::host::{HostCapacity, HostRecord, HostStatus};
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::session::{SessionMode, SessionSpec, SessionState};
use engram_core::types::session_op::{EnqueueOutcome, OpKind, SessionOp};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::teleport::*;
use engram_core::{HostId, SandboxError, SandboxId, SessionId, SnapshotId};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

pub struct ScriptedHost {
    pub sandbox: SandboxId,
    pub snapshot: SnapshotId,
    pub clock: Arc<dyn Clock>,
    pub capture_fails: AtomicBool,
    pub restore_fails: AtomicBool,
    pub resume_fails: AtomicBool,
    /// `resume` answers NotFound: the source sandbox no longer exists.
    pub resume_not_found: AtomicBool,
    /// `migration_abort` answers NotFound: the export is already consumed.
    pub abort_not_found: AtomicBool,
    pub destroy_fails: AtomicBool,
    pub spawn_fails: AtomicBool,
    pub captures: AtomicUsize,
    pub restores: AtomicUsize,
    pub resumes: AtomicUsize,
    pub aborts: AtomicUsize,
    pub destroys: AtomicUsize,
    pub spawns: AtomicUsize,
    pub presetups: AtomicUsize,
    pub live_refused: AtomicBool,
    pub live_lost: AtomicBool,
    pub peer_lost: AtomicBool,
    pub block_capture: AtomicBool,
    pub capture_entered: tokio::sync::Notify,
}
impl ScriptedHost {
    fn new(clock: Arc<dyn Clock>, entropy: &dyn Entropy) -> Arc<Self> {
        Arc::new(Self {
            sandbox: SandboxId::from(entropy.uuid()),
            snapshot: SnapshotId::from(entropy.uuid()),
            clock,
            capture_fails: AtomicBool::new(false),
            restore_fails: AtomicBool::new(false),
            resume_fails: AtomicBool::new(false),
            resume_not_found: AtomicBool::new(false),
            abort_not_found: AtomicBool::new(false),
            destroy_fails: AtomicBool::new(false),
            spawn_fails: AtomicBool::new(false),
            captures: AtomicUsize::new(0),
            restores: AtomicUsize::new(0),
            resumes: AtomicUsize::new(0),
            aborts: AtomicUsize::new(0),
            destroys: AtomicUsize::new(0),
            spawns: AtomicUsize::new(0),
            presetups: AtomicUsize::new(0),
            live_refused: AtomicBool::new(false),
            live_lost: AtomicBool::new(false),
            peer_lost: AtomicBool::new(false),
            block_capture: AtomicBool::new(false),
            capture_entered: tokio::sync::Notify::new(),
        })
    }
}
#[async_trait]
impl HostClient for ScriptedHost {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        Ok(self.sandbox)
    }
    async fn destroy(
        &self,
        _id: SandboxId,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        self.destroys.fetch_add(1, Ordering::SeqCst);
        if self.destroy_fails.load(Ordering::SeqCst) {
            return Err(SandboxError::Timeout);
        }
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
    async fn snapshot_hold(
        &self,
        id: SandboxId,
        fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.pause(id, fence).await?;
        self.snapshot(id, fence).await
    }

    async fn snapshot(
        &self,
        _id: SandboxId,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        if self.capture_fails.load(Ordering::SeqCst) {
            return Err(SandboxError::Snapshot("capture failed".into()));
        }
        Ok(SnapshotMetadata {
            id: self.snapshot,
            size_bytes: 1024,
            created_at: self.clock.now_utc(),
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
        self.restores.fetch_add(1, Ordering::SeqCst);
        if self.restore_fails.load(Ordering::SeqCst) {
            return Err(SandboxError::Snapshot("restore failed".into()));
        }
        Ok(self.sandbox)
    }
    async fn start_agent(
        &self,
        _id: SandboxId,
        _agent: engram_core::types::sandbox::AgentSpec,
        _policy: engram_core::types::egress::SessionEgressPolicy,
        _fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        if self.spawn_fails.load(Ordering::SeqCst) {
            return Err(SandboxError::InvalidSpec("spawn rejected".into()));
        }
        Ok(())
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
    async fn pause(&self, _id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn resume(&self, _id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        self.resumes.fetch_add(1, Ordering::SeqCst);
        if self.resume_not_found.load(Ordering::SeqCst) {
            Err(SandboxError::NotFound)
        } else if self.resume_fails.load(Ordering::SeqCst) {
            Err(SandboxError::Timeout)
        } else {
            Ok(())
        }
    }
    async fn migration_presetup(
        &self,
        _id: SandboxId,
        _fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        self.presetups.fetch_add(1, Ordering::SeqCst);
        if self.live_refused.load(Ordering::SeqCst) {
            return Err(SandboxError::InvalidSpec("swap enabled".into()));
        }
        let manifest = engram_core::types::manifest::ManifestRef {
            manifest_id: self.snapshot.as_uuid(),
            version: 1,
        };
        Ok(engram_core::types::snapshot::MigrationPresetupOut {
            export_id: "export-one".into(),
            peer_token: "token-one".into(),
            peer_port: 9000,
            sidecar_json: vec![1],
            memory_manifest_json: vec![2],
            memory_manifest_ref: manifest,
            disk_manifest_ref: Some(manifest),
            hot_chunks: vec![[3; 32]],
        })
    }
    async fn migration_capture_postcopy(
        &self,
        _id: SandboxId,
        export: &str,
        _fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, SandboxError> {
        assert_eq!(export, "export-one");
        self.capture_entered.notify_one();
        if self.block_capture.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.live_lost.load(Ordering::SeqCst) {
            return Err(SandboxError::NotFound);
        }
        Ok(engram_core::types::snapshot::PostCopyCaptureOut {
            sealed_chunks: 1,
            total_chunks: 1,
            pause_ms: 0,
            disk_drain_ms: 0,
            vmstate_ms: 0,
            scan_ms: 0,
            sealed_disk_chunks: 1,
            paused_at_unix_ms: 0,
        })
    }
    async fn migration_drain_wait(
        &self,
        _id: SandboxId,
    ) -> Result<engram_core::types::snapshot::DrainOutcome, SandboxError> {
        if self.peer_lost.load(Ordering::SeqCst) {
            Ok(engram_core::types::snapshot::DrainOutcome::PeerLost {
                remaining: 1,
                detail: "peer lost".into(),
            })
        } else {
            Ok(engram_core::types::snapshot::DrainOutcome::Done {
                pulled: 1,
                alt_sourced: 0,
                zero_chunks: 0,
                ms: 0,
            })
        }
    }
    async fn migration_commit(
        &self,
        _id: SandboxId,
        _export: &str,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        Ok(())
    }
    /// The live-move counterpart of `resume`: the same "source does not
    /// answer" flag keeps a rollback pending until the abort is acknowledged.
    async fn migration_abort(
        &self,
        _id: SandboxId,
        _export: &str,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        if self.abort_not_found.load(Ordering::SeqCst) {
            Err(SandboxError::NotFound)
        } else if self.resume_fails.load(Ordering::SeqCst) {
            Err(SandboxError::Timeout)
        } else {
            Ok(())
        }
    }
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }
}

pub fn host_record(id: HostId, name: &str, now: chrono::DateTime<chrono::Utc>) -> HostRecord {
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
        host_addr: Some("http://source:8080".into()),
        ready_images: Vec::new(),
        current_bundles: Vec::new(),
        sandbox_bundles: Vec::new(),
        cordoned: false,
        cordon_owner: None,
        cordon_reason: None,
        retire_requested_at: None,
        retired_at: None,
        total_vcpus: 16,
        wire_version: 1,
        stages_images: false,
        capabilities: Default::default(),
        lease_expires_at: None,
        lease_state: Default::default(),
        lease_epoch: 0,
    }
}

pub struct Rig {
    pub state: SharedState,
    pub source: Arc<ScriptedHost>,
    pub dest: Arc<ScriptedHost>,
    pub row: TeleportRow,
    pub op: SessionOp,
    pub clock: Arc<engram_sim::ManualClock>,
    pub _dir: tempfile::TempDir,
}
impl Rig {
    pub async fn new(meta: Arc<dyn MetadataStore>, clock: Arc<engram_sim::ManualClock>) -> Self {
        let entropy: Arc<dyn Entropy> = Arc::new(engram_sim::SimEntropy::seeded(77));
        let source = ScriptedHost::new(clock.clone(), entropy.as_ref());
        let dest = ScriptedHost::new(clock.clone(), entropy.as_ref());
        let source_id = HostId::from(entropy.uuid());
        let dest_id = HostId::from(entropy.uuid());
        meta.upsert_host(host_record(source_id, "source", clock.now_utc()))
            .await
            .unwrap();
        meta.upsert_host(host_record(dest_id, "dest", clock.now_utc()))
            .await
            .unwrap();
        let session = meta
            .create_session(SessionSpec {
                image: "test:teleport".into(),
                mode: SessionMode::DevVm,
            })
            .await
            .unwrap();
        meta.assign_session_host(session, Some(source_id))
            .await
            .unwrap();
        meta.transition_session_created(session, source.sandbox)
            .await
            .unwrap();
        meta.transition_session(
            session,
            SessionState::Active,
            engram_core::types::BindingDisposition::Retain,
        )
        .await
        .unwrap();
        let EnqueueOutcome::Claimed(op) = meta
            .op_enqueue_and_claim(
                session,
                OpKind::Teleport,
                serde_json::json!({}),
                None,
                "test",
            )
            .await
            .unwrap()
        else {
            panic!("claimed")
        };
        let TeleportAdmitOutcome::Admitted(row) = meta
            .teleport_admit(TeleportAdmitRequest {
                id: engram_core::TeleportId::from(entropy.uuid()),
                session_id: session,
                reason: TeleportReason::Ui,
                epoch: op.epoch.unwrap(),
                candidates: vec![dest_id],
                pinned_dest: Some(dest_id),
                mem_budget_mib: 128,
                cpu_budget_vcpus: 1,
                max_open_per_dest: 1,
                live_capable: false,
            })
            .await
            .unwrap()
        else {
            panic!("admitted")
        };
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(HostRegistry::new(meta.clone()));
        registry.register(source_id, source.clone());
        registry.register(dest_id, dest.clone());
        registry.record_sandbox_owner(source.sandbox, source_id);
        let blob = Arc::new(engram_storage_local::LocalBlobStorage::new(
            dir.path().join("blobs"),
        ));
        let services = Services {
            meta,
            host: registry.clone(),
            secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0; 32], "test",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: blob.clone(),
            chunk_store: engram_chunk_store::ChunkStore::new(blob),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: clock.clone(),
            entropy,
        };
        let state = Arc::new(AppState::new_with_registry(
            CoordinatorConfig {
                local_path: dir.path().to_path_buf(),
                ..Default::default()
            },
            services,
            registry,
        ));
        Self {
            state,
            source,
            dest,
            row: *row,
            op,
            clock,
            _dir: dir,
        }
    }
    pub async fn sim() -> Self {
        let clock = engram_sim::ManualClock::new();
        let meta = engram_sim::SimMetadataStore::new(
            clock.clone(),
            Arc::new(engram_sim::SimEntropy::seeded(88)),
        );
        Self::new(meta, clock).await
    }
    pub async fn drive(&self) -> coordinator::session_ops::OpOutcome {
        coordinator::session_verbs::dispatch(&coordinator::session_ops::OpCtx {
            state: &self.state,
            op: &self.op,
            epoch: self.op.epoch.unwrap(),
        })
        .await
    }
    pub async fn make_live(&self) {
        assert!(self
            .state
            .services
            .meta
            .teleport_advance(
                self.row.id,
                TeleportPhase::Admitted,
                TeleportPhase::Admitted,
                TeleportPatch {
                    kind: Some(TeleportKind::Live),
                    ..Default::default()
                },
                self.op.epoch.unwrap()
            )
            .await
            .unwrap());
    }
    pub async fn reclaim(&mut self) {
        self.clock.advance(std::time::Duration::from_secs(1));
        self.op = self
            .state
            .services
            .meta
            .op_reclaim_stale(std::time::Duration::ZERO, "successor")
            .await
            .unwrap()
            .into_iter()
            .find(|op| op.session_id == self.row.session_id)
            .expect("reclaimed");
    }
    pub fn spawn_drive(&self) -> tokio::task::JoinHandle<coordinator::session_ops::OpOutcome> {
        let state = self.state.clone();
        let op = self.op.clone();
        tokio::spawn(async move {
            coordinator::session_verbs::dispatch(&coordinator::session_ops::OpCtx {
                state: &state,
                epoch: op.epoch.unwrap(),
                op: &op,
            })
            .await
        })
    }
    pub async fn phase(&self) -> Option<TeleportPhase> {
        self.state
            .services
            .meta
            .open_teleport_for_session(self.row.session_id)
            .await
            .unwrap()
            .map(|r| r.phase)
    }
}
