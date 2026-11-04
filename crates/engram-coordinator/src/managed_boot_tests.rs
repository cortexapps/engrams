//! Synthetic host fault tests. Public admission and actual host execution are not exercised.
use super::*;
use async_trait::async_trait;
use engram_core::types::sandbox::ExecRequest as HostExecRequest;
use engram_core::{
    traits::{HostClient, MetadataStore, SessionFence},
    types::{
        egress::SessionEgressPolicy, managed_cold::*, managed_create::*, managed_deny::*,
        mcp_policy::*, runtime_spec::RuntimeSpec, sandbox::*, snapshot::SnapshotMetadata,
    },
    HostId, SandboxError, SandboxId,
};
use engram_sim::{ManualClock, MemBlobStorage, SimEntropy, SimMetadataStore};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
struct LossyHost {
    prepare_stall: Mutex<Option<SessionId>>,
    start_stall: AtomicBool,
    read_stall: AtomicBool,
    chunks: engram_chunk_store::ChunkStore,
    captures: AtomicUsize,
    state: Mutex<Option<ManagedBootHostState>>,
    starts: AtomicUsize,
    policy_calls: AtomicUsize,
    policy: Mutex<Option<ManagedAppliedState>>,
    prepared: ManagedBootPreparedReceipt,
    apply_mode: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
#[async_trait]
impl HostClient for LossyHost {
    fn harness_dial(&self) -> engram_core::traits::HarnessDial {
        engram_core::traits::HarnessDial::Vsock
    }
    async fn capture_managed_boot(
        &self,
        request: ManagedCaptureRequest,
    ) -> Result<ManagedCaptureAuthority, SandboxError> {
        let prepared = match self.state.lock().unwrap().clone() {
            Some(ManagedBootHostState::Captured { authority }) if authority.request == request => {
                return Ok(authority)
            }
            Some(ManagedBootHostState::Prepared { receipt })
                if receipt.request.key == request.key =>
            {
                *receipt
            }
            _ => return Err(SandboxError::Unavailable("source unavailable".into())),
        };
        let manifest = self
            .chunks
            .get_manifest(prepared.request.cold.disk.manifest)
            .await
            .map_err(|e| SandboxError::Vm(e.to_string().into()))?;
        self.chunks
            .put_manifest(request.disk_manifest(), &manifest)
            .await
            .map_err(|e| SandboxError::Vm(e.to_string().into()))?;
        let mut authority = ManagedCaptureAuthority {
            version: 1,
            disk_manifest: request.disk_manifest(),
            request,
            host_incarnation: prepared.host_incarnation,
            disk_sha256: prepared.request.cold.disk.sha256,
            disk_size_bytes: prepared.request.cold.disk.size_bytes,
            image: prepared.request.cold.image,
            kernel_sha256: prepared.request.cold.kernel_sha256,
            aux_ro_drives: prepared.request.cold.aux_ro_drives,
            consistency: ManagedDiskConsistency::CrashConsistentDiskCutV1,
            source_state: ManagedCapturedSourceState::Destroyed,
            record_digest: String::new(),
        };
        authority.record_digest = authority.digest().unwrap();
        self.captures.fetch_add(1, Ordering::SeqCst);
        *self.state.lock().unwrap() = Some(ManagedBootHostState::Captured { authority });
        Err(SandboxError::Unavailable(
            "capture reply lost after artifact commit".into(),
        ))
    }
    async fn read_managed_cold_assets(
        &self,
        request: engram_core::types::managed_assets::ManagedColdAssetsRequest,
    ) -> Result<engram_core::types::managed_assets::ManagedColdAssetsReceipt, SandboxError> {
        request
            .validate()
            .map_err(|e| SandboxError::Vm(e.to_string().into()))?;
        Ok(
            engram_core::types::managed_assets::ManagedColdAssetsReceipt {
                request,
                host_incarnation: "host".into(),
                backend: engram_core::types::managed_assets::ManagedColdBackend::Firecracker,
                kernel_sha256: "c".repeat(64),
                disk_sha256: "b".repeat(64),
                disk_size_bytes: 4096,
            },
        )
    }
    async fn prepare_managed_boot(
        &self,
        request: ManagedBootPrepare,
    ) -> Result<ManagedBootPreparedReceipt, SandboxError> {
        if *self.prepare_stall.lock().unwrap() == Some(request.key.creation.session_id) {
            return std::future::pending().await;
        }
        if let Some(ManagedBootHostState::Prepared { receipt }) =
            self.state.lock().unwrap().as_ref()
        {
            if receipt.request == request {
                return Ok(*receipt.clone());
            }
        }
        request
            .spec()
            .map_err(|e| SandboxError::Vm(e.to_string().into()))?;
        let receipt = ManagedBootPreparedReceipt {
            request,
            host_incarnation: "host".into(),
            network_identity: digest(b"network"),
            guest_ip: "192.0.2.2".parse().unwrap(),
            receipt_id: digest(b"prepared"),
        };
        *self.state.lock().unwrap() = Some(ManagedBootHostState::Prepared {
            receipt: Box::new(receipt),
        });
        Err(SandboxError::Unavailable("reply lost after prepare".into()))
    }
    async fn read_managed_policy(
        &self,
        _: ManagedPolicyTarget,
    ) -> Result<Option<ManagedAppliedState>, SandboxError> {
        self.policy_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.policy.lock().unwrap().clone())
    }
    async fn apply_managed_policy(
        &self,
        request: ManagedPolicyApply,
    ) -> Result<ManagedAppliedState, SandboxError> {
        self.policy_calls.fetch_add(1, Ordering::SeqCst);
        let mode = self.apply_mode.load(Ordering::SeqCst);
        if mode == 0 {
            return Err(SandboxError::Unavailable("policy unavailable".into()));
        }
        assert_eq!(*self.policy.lock().unwrap(), request.expected);
        assert_eq!(
            request.context.target,
            self.prepared.request.key.context.target
        );
        assert!(
            request.context.operator_generation
                > self.prepared.request.key.context.operator_generation
        );
        assert_eq!(request.egress.guest_ip, self.prepared.guest_ip);
        if request.expected.is_none() {
            assert!(request.context.binding_hash.is_none());
        }
        self.entered.notify_one();
        if mode == 2 {
            let _permit = self.release.acquire().await.unwrap();
        }
        let retained = request.egress.managed_deny.unwrap();
        assert!(retained.contains(&self.prepared.request.bootstrap_policy));
        *self.state.lock().unwrap() = Some(ManagedBootHostState::Retired {
            receipt: Box::new(ManagedBootRetiredReceipt {
                prepared: self.prepared.clone(),
                retained_policy: retained,
                receipt_id: digest(b"operator retired"),
            }),
        });
        Err(SandboxError::Unavailable(
            "reply lost after retirement".into(),
        ))
    }
    async fn start_managed_boot(
        &self,
        start: ManagedBootStart,
    ) -> Result<ManagedBootHostState, SandboxError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        *self.state.lock().unwrap() = Some(ManagedBootHostState::Started { intent: start });
        if self.start_stall.load(Ordering::SeqCst) {
            return std::future::pending().await;
        }
        Err(SandboxError::Unavailable("reply lost after spawn".into()))
    }
    async fn read_managed_boot(
        &self,
        _: ManagedBootKey,
    ) -> Result<ManagedBootHostState, SandboxError> {
        if self.read_stall.load(Ordering::SeqCst) {
            return std::future::pending().await;
        }
        self.state
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| SandboxError::Unavailable("host state unavailable".into()))
    }
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        unreachable!("exec-core tests never create sandboxes")
    }

    async fn destroy(&self, _id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        unreachable!("exec-core tests never destroy sandboxes")
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        unreachable!("exec-core tests never list sandboxes")
    }

    async fn probe_sandbox(&self, _id: SandboxId) -> Result<SandboxProbe, SandboxError> {
        unreachable!("exec-core tests never probe sandboxes")
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: HostExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let _ = (id, cmd);
        unreachable!("readiness never executes commands")
    }

    async fn snapshot(
        &self,
        _id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        unreachable!("exec-core tests never snapshot sandboxes")
    }

    async fn restore(
        &self,
        _metadata: SnapshotMetadata,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        unreachable!("exec-core tests never restore sandboxes")
    }

    async fn start_agent(
        &self,
        _id: SandboxId,
        _agent: AgentSpec,
        _policy: SessionEgressPolicy,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        unreachable!("policy control never starts guests")
    }

    async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
        unreachable!("boot scanner does not resolve another network")
    }

    async fn bind_session(
        &self,
        _session_id: SessionId,
        _sandbox_id: SandboxId,
        _binding_epoch: u64,
    ) -> Result<(), engram_core::SandboxError> {
        Err(SandboxError::Vm("bind rejected".into()))
    }

    async fn unbind_session(&self, _session_id: SessionId) {}

    async fn send_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
        _text: String,
        _mode: Option<String>,
    ) -> Result<(), SandboxError> {
        unreachable!("exec-core tests never send prompts")
    }
}

async fn fixture() -> (
    SharedState,
    Arc<SimMetadataStore>,
    Arc<LossyHost>,
    ManagedBootRecord,
) {
    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(700));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let sid = SessionId::from(uuid::Uuid::from_u128(700));
    let sbx = SandboxId::from(uuid::Uuid::from_u128(701));
    let host_id = HostId::from(uuid::Uuid::from_u128(702));
    let policy = McpPolicyCreation {
        actor_id: "owner".into(),
        input_id: "create".into(),
        parent_session_id: None,
        eligibility: WriteEligibility::ReadOnly,
    };
    let creation = ManagedCreation {
        identity: ManagedCreateIdentity {
            session_id: sid,
            task_id: "task".into(),
            policy: policy.clone(),
            input_digest: digest(b"input"),
        },
        native: FrozenNativeManifest {
            version: 1,
            bytes: b"[]".to_vec(),
            digest: digest(b"[]"),
        },
    };
    let ws = engram_core::traits::SessionCreateWriteSet {
        managed_boot_selection: None,
        managed_creation: Some(creation.clone()),
        managed_initial_prompt: None,
        mcp_policy: Some(policy),
        nbd_slot_need: 1,
        session_id: sid,
        spec: engram_core::types::SessionSpec {
            image: "test.invalid/managed:latest".into(),
            mode: Default::default(),
        },
        mem_budget_mib: 128,
        cpu_budget_vcpus: 1,
        sealed_secrets: None,
        capabilities: vec![],
        integration_policy_json: None,
        runtime_spec: RuntimeSpec::new(vec![], Some("claude".into()), None, vec![]),
        oauth_binding: None,
    };
    engram_testkit::operator::publish_empty(meta.as_ref()).await;
    meta.reserve_and_persist_create(ws, &[], 0).await.unwrap();
    let disk = engram_core::types::manifest::ManifestRef {
        manifest_id: uuid::Uuid::from_u128(703),
        version: 1,
    };
    let drives = (0..3)
        .map(|i| AuxRoDrive {
            drive_id: AuxRoDrive::slot_drive_id(i),
            guest_mount: AuxRoDrive::slot_guest_mount(i),
            fs_type: "squashfs".into(),
            sha256: Some("a".repeat(64)),
        })
        .collect::<Vec<_>>();
    let spec = SandboxSpec {
        image: "test.invalid/managed:latest".into(),
        rootfs_source: Some("disk.ext4".into()),
        image_uri: None,
        rootfs_manifest: Some(disk),
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: Default::default(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: drives.clone(),
        swap_mib: None,
        swap_source: None,
        swap_manifest: None,
    };
    let key = ManagedBootKey {
        creation: creation.identity,
        context: ManagedPolicyContext {
            target: ManagedPolicyTarget {
                session_id: sid,
                sandbox_id: sbx,
                host_id,
                binding_epoch: 1,
            },
            operator_generation: 1,
            operator_identity: engram_testkit::operator::candidate().identity().unwrap(),
            binding_hash: None,
        },
        boot_attempt: "boot-one".into(),
        native_digest: creation.native.digest.clone(),
    };
    let request = ManagedBootPrepare {
        key: key.clone(),
        capture_authority: None,
        bootstrap_policy: ManagedDenyPolicy::new(1, ["tools.example".into()]).unwrap(),
        sandbox_spec_json: serde_json::to_string(&spec).unwrap(),
        cold: ManagedColdPrepare {
            version: 1,
            session_id: sid,
            sandbox_id: sbx,
            boot_attempt: key.boot_attempt.clone(),
            image: spec.image,
            disk: ManagedColdDisk {
                manifest: disk,
                sha256: "b".repeat(64),
                size_bytes: 4096,
                source: ManagedColdSource::FreshImage {
                    base_manifest: engram_core::types::manifest::ManifestRef {
                        manifest_id: uuid::Uuid::from_u128(704),
                        version: 1,
                    },
                },
            },
            kernel_sha256: "c".repeat(64),
            aux_ro_drives: drives,
        },
    };
    request.spec().unwrap();
    // Explicit store fixture: exercise scanner authority over a durable placement.
    // Actual publication and host receipts have separate integration gates.
    meta.with_db_mut(|db| {
        db.managed_operator_gate = Some((1, key.context.operator_identity.clone(), true));
        let s = db.sessions.get_mut(&sid).unwrap();
        s.session.status = engram_core::types::SessionState::Created;
        s.session.host_id = Some(host_id);
        s.session.sandbox_id = Some(sbx);
        s.binding_epoch = 1;
        db.managed_boots
            .insert(sid, ManagedBootRecord::claimed(request.clone()));
    });
    let prepared = ManagedBootPreparedReceipt {
        request,
        host_incarnation: "host".into(),
        network_identity: digest(b"network"),
        guest_ip: "192.0.2.2".parse().unwrap(),
        receipt_id: digest(b"prepared"),
    };
    let row = meta
        .advance_managed_boot(
            &key,
            1,
            ManagedBootAdvance::Prepared(Box::new(prepared.clone())),
        )
        .await
        .unwrap();
    let mut context = key.context.clone();
    context.binding_hash = Some(digest(b"binding"));
    let applied = ManagedAppliedState::new(
        context.clone(),
        prepared.request.bootstrap_policy.clone(),
        "host".into(),
        "proxy".into(),
        Some(ManagedNetworkProof {
            target: context.target,
            guest_ip: prepared.guest_ip,
            network_identity: prepared.network_identity.clone(),
        }),
    )
    .unwrap();
    let write_policy = meta.read_mcp_write_policy(sid).await.unwrap();
    let f = ManagedBootFinalization {
        prepared: prepared.clone(),
        policy: applied,
        native_version: 1,
        combined_manifest: b"[]".to_vec(),
        combined_manifest_digest: digest(b"[]"),
        agent_digest: digest(b"fixture agent"),
    };
    let row = meta
        .advance_managed_boot(
            &key,
            row.revision,
            ManagedBootAdvance::Finalize {
                finalization: Box::new(f.clone()),
                write_policy,
                sealed_agent: ManagedBootSealedAgent {
                    wrapped_dek: vec![1],
                    nonce: vec![2; 12],
                    ciphertext: vec![3],
                    key_id: "fixture".into(),
                },
            },
        )
        .await
        .unwrap();
    let receipt = ManagedBootFinalReceipt {
        key: key.clone(),
        prepared_receipt_id: prepared.receipt_id,
        finalization_digest: f.digest().unwrap(),
        host_incarnation: prepared.host_incarnation,
        network_identity: prepared.network_identity,
        receipt_id: digest(b"final"),
    };
    let row = meta
        .advance_managed_boot(&key, row.revision, ManagedBootAdvance::Finalized(receipt))
        .await
        .unwrap();
    let blob = Arc::new(MemBlobStorage::new());
    let host = Arc::new(LossyHost {
        prepare_stall: Mutex::new(None),
        start_stall: AtomicBool::new(false),
        read_stall: AtomicBool::new(false),
        chunks: engram_chunk_store::ChunkStore::new(blob.clone()),
        captures: AtomicUsize::new(0),
        state: Mutex::new(Some(ManagedBootHostState::Finalized {
            receipt: row.receipt.clone().unwrap(),
        })),
        starts: AtomicUsize::new(0),
        policy_calls: AtomicUsize::new(0),
        policy: Mutex::new(Some(row.finalization.as_ref().unwrap().policy.clone())),
        prepared: row.prepared.clone().unwrap(),
        apply_mode: AtomicUsize::new(0),
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(0),
    });
    let services = crate::Services {
        meta: meta.clone(),
        host: host.clone(),
        host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
        secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
        kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0; 32], "fixture",
        )),
        oci: Arc::new(engram_oci::OciClient::new(Arc::new(
            engram_oci::AnonymousResolver,
        ))),
        auth_resolver: Arc::new(engram_oci::AnonymousResolver),
        blob: blob.clone(),
        chunk_store: engram_chunk_store::ChunkStore::new(blob),
        materialize_dir: None,
        clock,
        entropy,
    };
    (
        Arc::new(crate::AppState::new(
            crate::CoordinatorConfig::default(),
            services,
        )),
        meta,
        host,
        row,
    )
}
#[tokio::test]
async fn managed_boot_lost_start_reply_is_read_only_reconciled() {
    let (state, meta, host, row) = fixture().await;
    let key = &row.request.key;
    assert!(drive(&state, key).await.is_err());
    assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        meta.read_managed_boot(key).await.unwrap().phase,
        ManagedBootPhase::StartIssued
    );
    assert!(gate(&state, key.creation.session_id, true).await.is_err());
    // A new scanner step has no in-memory authority and recovers only the exact intent.
    run_once(&state).await.unwrap();
    assert_eq!(
        meta.read_managed_boot(key).await.unwrap().phase,
        ManagedBootPhase::Started
    );
    run_once(&state).await.unwrap();
    assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    gate(&state, key.creation.session_id, true).await.unwrap();
    assert!(gate(&state, key.creation.session_id, false).await.is_err());
    let retired = ManagedBootRetiredReceipt {
        prepared: row.prepared.unwrap(),
        retained_policy: ManagedDenyPolicy::new(2, ["tools.example".into(), "next.example".into()])
            .unwrap(),
        receipt_id: digest(b"retired"),
    };
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Retired {
        receipt: Box::new(retired.clone()),
    });
    run_once(&state).await.unwrap();
    let closed = meta.read_managed_boot(key).await.unwrap();
    assert_eq!(closed.phase, ManagedBootPhase::Interrupted);
    assert_eq!(closed.retired, Some(retired));
    assert!(gate(&state, key.creation.session_id, true).await.is_err());
    assert_eq!(host.starts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn managed_boot_missing_or_changed_host_state_never_reissues_start() {
    let (state, meta, host, row) = fixture().await;
    let key = &row.request.key;
    let start = ManagedBootStart {
        receipt: row.receipt.unwrap(),
        start_id: digest(b"start"),
    };
    meta.advance_managed_boot(
        key,
        row.revision,
        ManagedBootAdvance::IssueStart(start.clone()),
    )
    .await
    .unwrap();
    // Crash after durable intent but before its RPC: absence cannot authorize a new spawn.
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Missing { key: key.clone() });
    assert!(drive(&state, key).await.is_err());
    let mut wrong = start.clone();
    wrong.start_id = digest(b"different");
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Started { intent: wrong });
    assert!(drive(&state, key).await.is_err());
    *host.state.lock().unwrap() = Some(ManagedBootHostState::StartPending { intent: start });
    drive(&state, key).await.unwrap();
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        meta.read_managed_boot(key).await.unwrap().phase,
        ManagedBootPhase::StartIssued
    );
    assert!(gate(&state, key.creation.session_id, true).await.is_err());
}

#[tokio::test]
async fn managed_boot_generic_entry_points_stay_closed() {
    let (state, meta, host, row) = fixture().await;
    let sid = row.request.key.creation.session_id;
    let sbx = row.request.key.context.target.sandbox_id;
    let fence = SessionFence::new(sid, 1);
    assert!(crate::api::snapshot::ensure_active(&state, sid)
        .await
        .is_err());
    assert!(crate::api::snapshot::snapshot_core(&state, sid)
        .await
        .is_err());
    assert!(crate::api::snapshot::resume_core(&state, sid)
        .await
        .is_err());
    assert!(
        crate::api::snapshot::reattach_harness_in_place(&state, sid, sbx, fence)
            .await
            .is_err()
    );
    assert!(crate::teleport::admit_for_rpc(&state, sid, None)
        .await
        .is_err());
    crate::api::prompt::send_prompt_core(
        &state,
        sid,
        "pending".into(),
        "later".into(),
        None,
        crate::api::prompt::PromptDelivery::Enqueue,
    )
    .await
    .unwrap();
    assert!(meta
        .outbox_get("pending")
        .await
        .unwrap()
        .unwrap()
        .delivered_at
        .is_none());
    assert!(!meta
        .op_pending_exists(sid, engram_core::types::session_op::OpKind::Deliver)
        .await
        .unwrap());
    let registry = crate::host_registry::HostRegistry::new(meta.clone());
    registry.register(row.request.key.context.target.host_id, host.clone());
    let exec = HostExecRequest {
        command: vec!["true".into()],
        stdin: None,
        env: Default::default(),
        workdir: None,
        timeout: None,
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    assert!(registry.exec_stream(sbx, exec).await.is_err());
    assert!(registry
        .send_prompt(sbx, "p".into(), "prompt".into(), None)
        .await
        .is_err());
    assert!(registry.snapshot(sbx, fence).await.is_err());
    assert!(registry.bind_session(sid, sbx, 1).await.is_err());
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn managed_boot_prestart_retirement_is_durably_observed() {
    for phase in [
        ManagedBootPhase::Prepared,
        ManagedBootPhase::Finalizing,
        ManagedBootPhase::Finalized,
    ] {
        let (state, meta, host, row) = fixture().await;
        let key = &row.request.key;
        // Store fixture selects the crash boundary; no actual host proof is minted.
        meta.with_db_mut(|db| {
            db.managed_boots
                .get_mut(&key.creation.session_id)
                .unwrap()
                .phase = phase
        });
        let receipt = ManagedBootRetiredReceipt {
            prepared: row.prepared.clone().unwrap(),
            retained_policy: row.finalization.as_ref().unwrap().policy.policy.clone(),
            receipt_id: digest(b"prestart-retired"),
        };
        *host.state.lock().unwrap() = Some(ManagedBootHostState::Retired {
            receipt: Box::new(receipt.clone()),
        });
        run_once(&state).await.unwrap();
        let closed = meta.read_managed_boot(key).await.unwrap();
        assert_eq!(closed.phase, ManagedBootPhase::Interrupted);
        assert_eq!(closed.retired, Some(receipt));
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn managed_boot_attach_environment_is_exact_and_sealed() {
    let (state, _, _, mut row) = fixture().await;
    let target = &row.request.key.context.target;
    let mut agent = AgentSpec {
        argv: vec!["harness".into()],
        env: [
            ("ENGRAM_TOOLS".into(), "[]".into()),
            ("ENGRAM_SANDBOX_ID".into(), target.sandbox_id.to_string()),
            (
                "ENGRAM_BINDING_EPOCH".into(),
                target.binding_epoch.to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        session_env: Default::default(),
        binding_epoch: target.binding_epoch,
        host_ca_pem: None,
    };
    // Producer replaces stale harness-local values with the durable claim.
    agent.env.insert("ENGRAM_SANDBOX_ID".into(), "stale".into());
    agent.env.insert("ENGRAM_BINDING_EPOCH".into(), "0".into());
    bind_agent_target(&mut agent, &row.request.key).unwrap();
    assert_eq!(
        agent.env["ENGRAM_SANDBOX_ID"],
        target.sandbox_id.to_string()
    );
    assert_eq!(
        agent.env["ENGRAM_BINDING_EPOCH"],
        target.binding_epoch.to_string()
    );
    for name in ["ENGRAM_SANDBOX_ID", "ENGRAM_BINDING_EPOCH"] {
        let mut override_agent = agent.clone();
        override_agent
            .session_env
            .insert(name.into(), agent.env[name].clone());
        assert!(bind_agent_target(&mut override_agent, &row.request.key).is_err());
    }
    let mut command = ManagedBootFinalize {
        finalization: row.finalization.clone().unwrap(),
        agent,
    };
    let canonical =
        |agent: &AgentSpec| serde_json::to_vec(&serde_json::to_value(agent).unwrap()).unwrap();
    command.finalization.agent_digest = digest(&canonical(&command.agent));
    command.validate_agent().unwrap();
    for name in ["ENGRAM_SANDBOX_ID", "ENGRAM_BINDING_EPOCH"] {
        for change in 0..3 {
            let mut wrong = command.clone();
            match change {
                0 => {
                    wrong.agent.env.remove(name);
                }
                1 => {
                    wrong.agent.env.insert(name.into(), "wrong".into());
                }
                _ => {
                    wrong
                        .agent
                        .session_env
                        .insert(name.into(), command.agent.env[name].clone());
                }
            }
            // Recompute the hash: valid integrity cannot authorize an incorrect target.
            wrong.finalization.agent_digest = digest(&canonical(&wrong.agent));
            assert!(
                wrong.validate_agent().is_err(),
                "accepted invalid {name}, case {change}"
            );
        }
    }
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let sealed = cipher.seal(&canonical(&command.agent)).await.unwrap();
    row.finalization = Some(command.finalization);
    row.sealed_agent = Some(ManagedBootSealedAgent {
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
    });
    let opened = open_finalize(&state, &row).await.unwrap();
    assert_eq!(opened.agent.env, command.agent.env);
    assert_eq!(opened.agent.session_env, command.agent.session_env);
}

#[tokio::test]
async fn managed_boot_event_entry_points_reject_interrupted_authority() {
    use crate::state::{emit_harness_event, SessionEvent};
    use engram_harness_proto::HarnessEvent;
    use engram_host_agent::harness::EventDelivery;
    let (state, meta, _, row) = fixture().await;
    let key = row.request.key.clone();
    let sid = key.creation.session_id;
    let sbx = key.context.target.sandbox_id;
    let start = ManagedBootStart {
        receipt: row.receipt.unwrap(),
        start_id: digest(b"event-start"),
    };
    let row = meta
        .advance_managed_boot(
            &key,
            row.revision,
            ManagedBootAdvance::IssueStart(start.clone()),
        )
        .await
        .unwrap();
    let row = meta
        .advance_managed_boot(&key, row.revision, ManagedBootAdvance::Started(start))
        .await
        .unwrap();
    let now = state.services.clock.now_utc();
    let delivery = Some(EventDelivery {
        binding_epoch: 1,
        incarnation: 1,
        seq: 1,
    });
    let run = || HarnessEvent::RunStarted {
        run_id: "old-run".into(),
        prompt_summary: None,
        prompt_id: None,
    };
    emit_harness_event(&state, sid, sbx, run(), now, delivery)
        .await
        .unwrap();
    // Replay through the other entry point uses the same transactional key.
    state
        .harness_hub
        .emit_external(sid, sbx, run(), delivery)
        .await
        .unwrap();
    let pending = engram_core::types::outbox::OutboxRow {
        prompt_id: "pending-boundary".into(),
        session_id: sid,
        kind: engram_core::types::outbox::OutboxKind::Prompt,
        payload: serde_json::json!({"text":"pending"}),
        created_at: now,
        not_before: now,
        attempts: 0,
        delivered_at: None,
        acked_at: None,
    };
    meta.outbox_enqueue(&pending).await.unwrap();
    let mut live = state.events.subscribe(sid);
    meta.advance_managed_boot(&key, row.revision, ManagedBootAdvance::Interrupt)
        .await
        .unwrap();
    let late = || HarnessEvent::RunStarted {
        run_id: "late-run".into(),
        prompt_summary: None,
        prompt_id: Some(pending.prompt_id.clone()),
    };
    let delivery = Some(EventDelivery {
        binding_epoch: 1,
        incarnation: 1,
        seq: 2,
    });
    assert!(emit_harness_event(&state, sid, sbx, late(), now, delivery)
        .await
        .is_err());
    assert!(state
        .harness_hub
        .emit_external(sid, sbx, late(), delivery)
        .await
        .is_err());
    assert!(live.try_recv().is_err());
    assert!(meta
        .outbox_get(&pending.prompt_id)
        .await
        .unwrap()
        .unwrap()
        .acked_at
        .is_none());
    let rows = meta.list_session_events_since(sid, -1, 100).await.unwrap();
    assert_eq!(rows.iter().filter(|r| r.kind == "run_started").count(), 1);
    let interrupted = rows
        .into_iter()
        .find(|r| r.kind == "run_interrupted")
        .unwrap();
    assert!(
        matches!(serde_json::from_value::<SessionEvent>(interrupted.payload).unwrap(),SessionEvent::HarnessRunInterrupted {run_id,..} if run_id=="old-run")
    );
}

#[tokio::test]
async fn managed_boot_interruption_publishes_through_pg_listener() {
    use crate::state::SessionEvent;
    use engram_core::types::host::{HostCapacity, HostRecord, HostStatus};
    use std::time::Duration;
    let Some(db) = engram_testkit::pg::fresh_db().await else {
        return;
    };
    let (state, sim, _, row) = fixture().await;
    let key = &row.request.key;
    let sid = key.creation.session_id;
    let target = &key.context.target;
    let now = state.services.clock.now_utc();
    let meta = Arc::new(db.store.clone());
    engram_testkit::operator::publish_empty(meta.as_ref()).await;
    let creation = sim.read_managed_creation(&key.creation).await.unwrap();
    meta.reserve_and_persist_create(
        engram_core::traits::SessionCreateWriteSet {
            managed_boot_selection: None,
            session_id: sid,
            spec: engram_core::types::SessionSpec {
                image: row.request.cold.image.clone(),
                mode: Default::default(),
            },
            mem_budget_mib: 128,
            cpu_budget_vcpus: 1,
            nbd_slot_need: 1,
            sealed_secrets: None,
            capabilities: vec![],
            integration_policy_json: None,
            runtime_spec: RuntimeSpec::new(vec![], Some("claude".into()), None, vec![]),
            oauth_binding: None,
            mcp_policy: Some(creation.identity.policy.clone()),
            managed_creation: Some(creation),
            managed_initial_prompt: None,
        },
        &[],
        0,
    )
    .await
    .unwrap();
    meta.upsert_host(HostRecord {
        id: target.host_id,
        hostname: "managed-host".into(),
        cloud_metadata: Default::default(),
        capacity: HostCapacity {
            total_gb: 0,
            used_gb: 0,
            total_mib: 1024,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: now,
        host_addr: None,
        ready_images: vec![],
        current_bundles: vec![],
        sandbox_bundles: vec![],
        cordoned: false,
        cordon_owner: None,
        cordon_reason: None,
        retire_requested_at: None,
        retired_at: None,
        total_vcpus: 2,
        wire_version: 0,
        stages_images: false,
        capabilities: Default::default(),
        lease_expires_at: None,
        lease_state: Default::default(),
        lease_epoch: 0,
    })
    .await
    .unwrap();
    // Explicit fixture imports a finalized source. This test proves event storage
    // and publication, not a real host receipt or public admission.
    sqlx::query("UPDATE sessions SET status='created',host_id=$2,sandbox_id=$3,binding_epoch=$4 WHERE id=$1")
        .bind(sid.as_uuid()).bind(target.host_id.as_uuid()).bind(target.sandbox_id.as_uuid()).bind(target.binding_epoch as i64)
        .execute(meta.pool()).await.unwrap();
    sqlx::query("INSERT INTO session_managed_boots(session_id,host_id,sandbox_id,binding_epoch,boot_attempt,revision,phase,record,updated_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(sid.as_uuid()).bind(target.host_id.as_uuid()).bind(target.sandbox_id.as_uuid()).bind(target.binding_epoch as i64)
        .bind(&key.boot_attempt).bind(row.revision as i64).bind(row.phase.as_str()).bind(serde_json::to_value(&row).unwrap()).bind(now)
        .execute(meta.pool()).await.unwrap();
    let mut live = state.events.subscribe(sid);
    let listener = crate::pg_listener::spawn(
        db.url.clone(),
        meta.clone(),
        state.events.clone(),
        state.host_registry.clone(),
        state.integrations.clone(),
        state.boot_bundles.clone(),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    // No local publish: receiving this marker proves LISTEN is established.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            meta.append_session_event(
                sid,
                "run_started",
                serde_json::to_value(SessionEvent::HarnessRunStarted {
                    run_id: "old-run".into(),
                    prompt_summary: None,
                    prompt_id: None,
                    at: now,
                })
                .unwrap(),
            )
            .await
            .unwrap();
            if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(50), live.recv()).await {
                break;
            }
        }
    })
    .await
    .expect("listener must receive readiness marker");
    meta.advance_managed_boot(key, row.revision, ManagedBootAdvance::Interrupt)
        .await
        .unwrap();
    let published = tokio::time::timeout(Duration::from_secs(3),async {
        loop { let event = live.recv().await.unwrap(); if matches!(&event.event,SessionEvent::HarnessRunInterrupted {run_id,..} if run_id=="old-run") { break event; } }
    }).await.expect("committed interruption must reach the live bus through NOTIFY");
    let stored = meta
        .list_session_events_since(sid, published.idx - 1, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(stored.kind, "run_interrupted");
    assert_eq!(stored.idx, published.idx);
    assert!(
        matches!(serde_json::from_value::<SessionEvent>(stored.payload).unwrap(),SessionEvent::HarnessRunInterrupted {run_id,..} if run_id=="old-run")
    );
    listener.abort();
    let _ = listener.await;
}

#[tokio::test]
async fn operator_scanner_requires_retirement_and_retains_before_host_io() {
    let (state, meta, host, row) = fixture().await;
    let mut candidate = engram_testkit::operator::candidate();
    candidate.network_identity = digest(b"new network same authority hosts");
    let g = meta
        .stage_operator_publication(Some(1), candidate.clone())
        .await
        .unwrap()
        .pending
        .unwrap();
    // Error and Missing are not proof. The scanner remains closed on both.
    *host.state.lock().unwrap() = None;
    crate::mcp::operator::run_once(&state, &candidate, None)
        .await
        .unwrap();
    assert!(
        !meta
            .read_operator_publication()
            .await
            .unwrap()
            .admission_open
    );
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Missing {
        key: row.request.key.clone(),
    });
    crate::mcp::operator::run_once(&state, &candidate, None)
        .await
        .unwrap();
    assert!(
        !meta
            .read_operator_publication()
            .await
            .unwrap()
            .admission_open
    );
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Finalized {
        receipt: row.receipt.clone().unwrap(),
    });
    host.apply_mode.store(2, Ordering::SeqCst);
    let scanner = tokio::spawn({
        let state = state.clone();
        let candidate = candidate.clone();
        async move { crate::mcp::operator::run_once(&state, &candidate, None).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), host.entered.notified())
        .await
        .unwrap();
    let retained = meta
        .get_session_integration_policy(row.request.key.creation.session_id)
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&retained).unwrap();
    assert_eq!(
        value["managed_deny"]["revision"], 2,
        "same-host generation must invalidate the old policy receipt"
    );
    // The same-row retention transaction can complete while the host is blocked.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        meta.bind_session_integration_policy(row.request.key.creation.session_id, &retained),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(meta.publish_operator_generation(&g).await.is_err());
    scanner.abort();
    let _ = scanner.await;
    assert!(
        !meta
            .read_operator_publication()
            .await
            .unwrap()
            .admission_open
    );
    // A fresh pass recovers the persistent candidate/union, obtains a fresh host
    // observation after a lost apply reply, and never asks the host to start.
    host.apply_mode.store(1, Ordering::SeqCst);
    crate::mcp::operator::run_once(&state, &candidate, None)
        .await
        .unwrap();
    assert!(
        meta.read_operator_publication()
            .await
            .unwrap()
            .admission_open
    );
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        meta.managed_boot_open(row.request.key.creation.session_id)
            .await
            .unwrap(),
        Some(false)
    );
}

#[tokio::test]
async fn operator_scanner_retires_closed_owner_without_fabricating_binding() {
    let (state, meta, host, row) = fixture().await;
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Prepared {
        receipt: Box::new(row.prepared.clone().unwrap()),
    });
    *host.policy.lock().unwrap() = None;
    host.apply_mode.store(1, Ordering::SeqCst);
    // Model a prepared reply not yet persisted: the registered claim still owns it.
    meta.with_db_mut(|db| {
        db.managed_boots.insert(
            row.request.key.creation.session_id,
            ManagedBootRecord::claimed(row.request.clone()),
        );
    });
    let mut candidate = engram_testkit::operator::candidate();
    candidate.network_identity = digest(b"new closed network");
    meta.stage_operator_publication(Some(1), candidate.clone())
        .await
        .unwrap();
    crate::mcp::operator::run_once(&state, &candidate, None)
        .await
        .unwrap();
    assert!(
        meta.read_operator_publication()
            .await
            .unwrap()
            .admission_open
    );
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn operator_scanner_keeps_policy_only_lineage_blocked_without_host_calls() {
    let (state, meta, host, row) = fixture().await;
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Retired {
        receipt: Box::new(ManagedBootRetiredReceipt {
            prepared: row.prepared.clone().unwrap(),
            retained_policy: row.request.bootstrap_policy.clone(),
            receipt_id: digest(b"permanent owner"),
        }),
    });
    let sid = meta
        .create_session(engram_core::types::SessionSpec {
            image: "test.invalid/policy-only:latest".into(),
            mode: Default::default(),
        })
        .await
        .unwrap();
    let hid = row.request.key.context.target.host_id;
    meta.assign_session_host(sid, Some(hid)).await.unwrap();
    let sandbox = SandboxId::from(uuid::Uuid::from_u128(991));
    let epoch = meta.transition_session_created(sid, sandbox).await.unwrap();
    let target = ManagedPolicyTarget {
        session_id: sid,
        host_id: hid,
        sandbox_id: sandbox,
        binding_epoch: epoch,
    };
    meta.retain_managed_deny(
        &target,
        &ManagedDenyPolicy::new(1, ["tools.example".into(), "history.example".into()]).unwrap(),
    )
    .await
    .unwrap();
    let candidate = engram_testkit::operator::candidate();
    let g = meta
        .stage_operator_publication(Some(1), candidate.clone())
        .await
        .unwrap()
        .pending
        .unwrap();
    for (host_inc, proxy_inc) in [
        ("old-host", "old-proxy"),
        ("next-host", "old-proxy"),
        ("next-host", "next-proxy"),
    ] {
        let receipt = ManagedAppliedState::new(
            ManagedPolicyContext {
                target: target.clone(),
                operator_generation: g.generation,
                operator_identity: g.identity.clone(),
                binding_hash: None,
            },
            ManagedDenyPolicy::new(2, ["tools.example".into(), "history.example".into()]).unwrap(),
            host_inc.into(),
            proxy_inc.into(),
            Some(ManagedNetworkProof {
                target: target.clone(),
                guest_ip: "192.0.2.2".parse().unwrap(),
                network_identity: digest(b"network"),
            }),
        )
        .unwrap();
        *host.policy.lock().unwrap() = Some(receipt);
        crate::mcp::operator::run_once(&state, &candidate, None)
            .await
            .unwrap();
        assert_eq!(
            host.policy_calls.load(Ordering::SeqCst),
            0,
            "unsupported lineage must not authorize host I/O"
        );
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
        assert!(
            !meta
                .read_operator_publication()
                .await
                .unwrap()
                .admission_open
        );
        meta.with_db_mut(|db| {
            assert!(
                db.operator_readiness
                    .get(&(g.generation, row.request.key.creation.session_id))
                    .unwrap()
                    .1
                    .is_some(),
                "current immutable source still reaches permanent readiness"
            );
            let (participant, proof) = db.operator_readiness.get(&(g.generation, sid)).unwrap();
            assert!(proof.is_none());
            assert!(participant.policy.denies("history.example"));
            assert!(participant.policy.denies("tools.example"));
            assert!(participant.boot.is_none());
        });
    }
}

#[tokio::test]
async fn managed_bridge_real_driver_claims_once_and_reconciles_prepared_reply() {
    managed_bridge_driver(false).await;
}

#[tokio::test]
async fn managed_bridge_driver_uses_held_two_slot_requirement() {
    managed_bridge_driver(true).await;
}

async fn managed_bridge_driver(two_slots: bool) {
    use engram_core::types::{host::*, managed_bridge::*};
    let (mut state, meta, host, row) = fixture().await;
    let sid = row.request.key.creation.session_id;
    let host_id = row.request.key.context.target.host_id;
    let base = match row.request.cold.disk.source {
        ManagedColdSource::FreshImage { base_manifest } => base_manifest,
        _ => unreachable!(),
    };
    let mut selection = ManagedBootSelection {
        launch: ManagedHarnessLaunch {
            descriptor: crate::builtin_harness::builtin("claude")
                .unwrap()
                .descriptor()
                .unwrap(),
            workdir: Some("/work".into()),
        },
        image: row.request.cold.image.clone(),
        image_digest: digest(b"approved image"),
        base_manifest: base,
        aux_ro_drives: row.request.cold.aux_ro_drives.clone(),
        vcpus: 1,
        memory_mib: 128,
        disk_gib: 1,
    };
    // Explicit held-reservation fixture. The test drives actual coordinator selection,
    // blob fork, claim and prepare code; its host is controlled, not physical proof.
    meta.with_db_mut(|db| {
        db.managed_boots.remove(&sid);
        let s = db.sessions.get_mut(&sid).unwrap();
        s.session.status = engram_core::types::SessionState::Queued;
        s.session.host_id = None;
        s.session.sandbox_id = None;
        s.binding_epoch = 0;
        if two_slots {
            s.nbd_slot_need = 2;
        }
        db.managed_boot_selections.insert(sid, selection.clone());
    });
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Missing {
        key: row.request.key.clone(),
    });
    meta.upsert_host(HostRecord {
        id: host_id,
        hostname: "bridge-host".into(),
        cloud_metadata: Default::default(),
        capacity: HostCapacity {
            total_gb: 10,
            used_gb: 0,
            total_mib: 8192,
            used_mib: 0,
            running_sandboxes: 0,
        },
        utilization: Default::default(),
        status: HostStatus::Ready,
        last_heartbeat_at: state.services.clock.now_utc(),
        host_addr: None,
        ready_images: vec![selection.image_digest.clone()],
        current_bundles: vec![],
        sandbox_bundles: vec![],
        cordoned: false,
        cordon_owner: None,
        cordon_reason: None,
        retire_requested_at: None,
        retired_at: None,
        total_vcpus: 4,
        wire_version: engram_protocol::WIRE_VERSION,
        stages_images: true,
        capabilities: HostCapabilities {
            backend: "firecracker".into(),
            ..Default::default()
        },
        lease_expires_at: None,
        lease_state: Default::default(),
        lease_epoch: 0,
    })
    .await
    .unwrap();
    let registered = meta.get_host(host_id).await.unwrap().unwrap();
    meta.touch_host_heartbeat(
        host_id,
        HostHeartbeat {
            status: HostStatus::Ready,
            capacity: registered.capacity,
            utilization: Default::default(),
            ready_images: vec![selection.image_digest.clone()],
            current_bundles: vec![],
            sandbox_bundles: vec![],
            total_vcpus: 4,
            wire_version: engram_protocol::WIRE_VERSION,
            stages_images: true,
            capabilities: registered.capabilities,
            lease_renew_until: None,
        },
    )
    .await
    .unwrap();
    let free_host = HostId::from(uuid::Uuid::from_u128(705));
    let mut second = meta.get_host(host_id).await.unwrap().unwrap();
    second.id = free_host;
    meta.upsert_host(second.clone()).await.unwrap();
    for (id, capacity) in [(host_id, 64), (free_host, 1024)] {
        meta.touch_host_heartbeat(
            id,
            HostHeartbeat {
                status: HostStatus::Ready,
                capacity: second.capacity.clone(),
                utilization: HostUtilization {
                    allocatable_mib: if two_slots { 1024 } else { capacity },
                    nbd_slots_total: if two_slots {
                        if id == host_id {
                            1
                        } else {
                            2
                        }
                    } else {
                        0
                    },
                    ..Default::default()
                },
                ready_images: vec![selection.image_digest.clone()],
                current_bundles: vec![],
                sandbox_bundles: vec![],
                total_vcpus: 4,
                wire_version: engram_protocol::WIRE_VERSION,
                stages_images: true,
                capabilities: second.capabilities.clone(),
                lease_renew_until: None,
            },
        )
        .await
        .unwrap();
    }
    // Resolve launch A through the real trusted catalog path, then retain it with the drive.
    meta.with_db_mut(|db| {
        db.hosts.get_mut(&host_id).unwrap().current_bundles = vec![AuxBundleRef {
            drive_id: "harness-claude".into(),
            sha256: "a".repeat(64),
        }]
    });
    let resolved = crate::api::sessions::resolve_harness(
        &state,
        Some("claude"),
        Default::default(),
        sid,
        [("ORIGINAL_SECRET".into(), "original-value".into())].into(),
        Some("/work".into()),
        Some("plan"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resolved.mount, selection.aux_ro_drives[0]);
    let original_argv = resolved.agent.argv.clone();
    selection.launch = resolved.launch;
    let sealed =
        crate::api::sessions::seal_session_secrets(&state, sid, &resolved.agent.session_env)
            .await
            .unwrap();
    meta.with_db_mut(|db| {
        db.managed_boot_selections.insert(sid, selection.clone());
        db.session_secrets.insert(sid, sealed.clone());
    });
    let manifest = engram_chunk_store::Manifest {
        schema_version: 1,
        kind: engram_chunk_store::ManifestKind::Disk,
        chunk_size: engram_chunk_store::ChunkSize::bytes(4096),
        total_bytes: 4096,
        chunks: vec![],
        parent: None,
        working_set_trace: None,
        annotations: serde_json::Value::Null,
    };
    state
        .services
        .chunk_store
        .put_manifest(base, &manifest)
        .await
        .unwrap();
    let creation = meta
        .read_managed_creation(&row.request.key.creation)
        .await
        .unwrap();
    let q = ManagedCreationQuery {
        identity: creation.identity,
        native_version: creation.native.version,
        native_digest: creation.native.digest,
    };
    let first = crate::managed_bridge::prepare(&state, q.clone())
        .await
        .unwrap();
    let retry = crate::managed_bridge::prepare(&state, q.clone())
        .await
        .unwrap();
    assert_eq!(first.operation, retry.operation);
    let initial = meta
        .read_managed_boot_intent(&q, "initial")
        .await
        .unwrap()
        .unwrap();
    let lost = Arc::new(LostManifestReply {
        inner: state.services.blob.clone(),
        target: initial.disk_manifest.storage_key(),
        lost: AtomicUsize::new(0),
    });
    Arc::get_mut(&mut state).unwrap().services.chunk_store =
        engram_chunk_store::ChunkStore::new(lost.clone());
    // Accepted prompts remain queued during preparation; no Deliver op may exist.
    crate::api::prompt::send_prompt_core(
        &state,
        sid,
        "held-prompt".into(),
        "queued".into(),
        None,
        crate::api::prompt::PromptDelivery::Enqueue,
    )
    .await
    .unwrap();
    assert!(!meta
        .op_pending_exists(sid, engram_core::types::session_op::OpKind::Deliver)
        .await
        .unwrap());
    for step in 0..5 {
        let current = meta
            .read_managed_boot_intent(&q, "initial")
            .await
            .unwrap()
            .unwrap();
        if step == 1 {
            assert_eq!(
                current.host_id,
                Some(free_host),
                "skip full first candidate"
            );
        }
        if step == 2 {
            assert_eq!(current.host_id, Some(free_host));
            assert!(current.assets.is_some());
            meta.set_host_cordon(
                free_host,
                Some(CordonOwner::Admin),
                Some("fixture host loss"),
            )
            .await
            .unwrap();
            meta.with_db_mut(|db| {
                let host = db.hosts.get_mut(&host_id).unwrap();
                host.utilization.allocatable_mib = 1024;
                if two_slots {
                    host.utilization.nbd_slots_total = 2;
                }
            });
        }
        if step == 3 {
            assert_eq!(current.host_id, Some(host_id));
            assert!(current.assets.is_none(), "old host assets are fenced");
            assert_eq!(current.disk_manifest, initial.disk_manifest);
            assert_eq!(current.sandbox_id, initial.sandbox_id);
        }
        crate::managed_bridge::drive(&state, current)
            .await
            .unwrap_or_else(|e| panic!("initial phase {step}: {e:?}"));
    }
    crate::managed_bridge::run_once(&state).await.unwrap();
    let claimed = meta.get_managed_boot(&q.identity).await.unwrap().unwrap();
    assert_eq!(claimed.request.key.boot_attempt, initial.intent_id);
    assert_eq!(
        claimed.request.key.context.target.sandbox_id,
        initial.sandbox_id
    );
    assert_eq!(claimed.phase, ManagedBootPhase::Claimed);
    let fork = state
        .services
        .chunk_store
        .get_manifest(initial.disk_manifest)
        .await
        .unwrap();
    assert_eq!(
        lost.lost.load(Ordering::SeqCst),
        1,
        "one committed private manifest despite a lost PUT reply"
    );
    assert_eq!(fork.parent, Some(base));
    assert_eq!(fork.total_bytes, manifest.total_bytes);
    // Replaying the old intent after a lost claim reply does not allocate or fork again.
    let committed = meta
        .read_managed_boot_intent(&q, "initial")
        .await
        .unwrap()
        .unwrap();
    assert!(committed.completed);
    crate::managed_bridge::drive(&state, committed)
        .await
        .unwrap();
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Missing {
        key: claimed.request.key.clone(),
    });
    assert!(drive(&state, &claimed.request.key).await.is_err());
    drive(&state, &claimed.request.key).await.unwrap();
    let prepared = meta.read_managed_boot(&claimed.request.key).await.unwrap();
    assert_eq!(prepared.phase, ManagedBootPhase::Prepared);
    assert!(prepared.sealed_agent.is_none());
    assert!(meta
        .outbox_get("held-prompt")
        .await
        .unwrap()
        .unwrap()
        .acked_at
        .is_none());
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    meta.with_db_mut(|db| {
        db.hosts.get_mut(&host_id).unwrap().current_bundles[0].sha256 = "f".repeat(64)
    });
    state.boot_bundles.invalidate_fleet_catalog();
    let (_, _, current_mount) = crate::api::sessions::resolve_harness_mount(&state, "claude")
        .await
        .unwrap();
    assert_ne!(
        current_mount, selection.aux_ro_drives[0],
        "current catalog is B"
    );
    let finalized = finalize_bridge_fixture(&state, &host, &prepared).await;
    let opened = open_finalize(&state, &finalized).await.unwrap();
    assert_eq!(opened.agent.argv, original_argv);
    assert_eq!(opened.finalization.combined_manifest_digest, digest(b"[]"));
    assert_eq!(
        opened.finalization.prepared.request.cold.aux_ro_drives,
        selection.aux_ro_drives
    );
    assert_eq!(
        opened.agent.session_env.get("ORIGINAL_SECRET").unwrap(),
        "original-value"
    );
    assert_eq!(
        opened
            .agent
            .env
            .get(engram_harness_proto::HARNESS_CWD_ENV)
            .unwrap(),
        "/work"
    );
    assert_eq!(
        meta.get_session_secrets(sid)
            .await
            .unwrap()
            .unwrap()
            .ciphertext,
        sealed.ciphertext
    );
    if two_slots {
        meta.set_host_cordon(free_host, None, None).await.unwrap();
    }
    let input = ManagedRecoveryInput {
        query: q.clone(),
        expected_source_key: claimed.request.key.clone(),
        recovery_id: "recover-one".into(),
    };
    let recovering = crate::managed_bridge::recover(&state, input.clone())
        .await
        .unwrap();
    let retry = crate::managed_bridge::recover(&state, input.clone())
        .await
        .unwrap();
    assert_eq!(recovering.operation, retry.operation);
    let recovery = meta
        .read_managed_boot_intent(&q, "recover:recover-one")
        .await
        .unwrap()
        .unwrap();
    assert!(
        crate::managed_bridge::drive(&state, recovery.clone())
            .await
            .is_err(),
        "lost capture reply"
    );
    assert_eq!(host.captures.load(Ordering::SeqCst), 1);
    assert!(meta
        .list_live_session_disk_manifest_ids()
        .await
        .unwrap()
        .contains(&recovery.disk_manifest));
    for _ in 0..3 {
        crate::managed_bridge::run_once(&state).await.unwrap();
    }
    let next = meta.get_managed_boot(&q.identity).await.unwrap().unwrap();
    assert_eq!(next.request.key.context.target.binding_epoch, 2);
    assert_eq!(next.request.cold.disk.manifest, recovery.disk_manifest);
    assert!(next.request.capture_authority.is_some());
    assert_eq!(host.captures.load(Ordering::SeqCst), 1);
    assert!(meta.read_managed_boot(&claimed.request.key).await.is_err());
    let reconciled = crate::managed_bridge::recover(&state, input).await.unwrap();
    assert!(reconciled.operation.unwrap().completed);
    assert_eq!(reconciled.boot.unwrap().request.key, next.request.key);
    assert!(
        drive(&state, &next.request.key).await.is_err(),
        "lost recovery prepare reply"
    );
    drive(&state, &next.request.key).await.unwrap();
    let recovered = meta.read_managed_boot(&next.request.key).await.unwrap();
    let final_recovery = finalize_bridge_fixture(&state, &host, &recovered).await;
    let opened = open_finalize(&state, &final_recovery).await.unwrap();
    assert_eq!(opened.agent.argv, original_argv);
    assert_eq!(
        opened.agent.session_env.get("ORIGINAL_SECRET").unwrap(),
        "original-value"
    );
    assert_eq!(
        opened.finalization.prepared.request.cold.aux_ro_drives,
        selection.aux_ro_drives
    );
    assert_eq!(
        meta.get_session_secrets(sid)
            .await
            .unwrap()
            .unwrap()
            .ciphertext,
        sealed.ciphertext
    );

    assert!(meta
        .outbox_get("held-prompt")
        .await
        .unwrap()
        .unwrap()
        .acked_at
        .is_none());
}

async fn finalize_bridge_fixture(
    state: &SharedState,
    host: &LossyHost,
    prepared: &ManagedBootRecord,
) -> ManagedBootRecord {
    let receipt = prepared.prepared.clone().unwrap();
    let mut context = prepared.request.key.context.clone();
    context.binding_hash = Some(digest(b"binding"));
    let applied = ManagedAppliedState::new(
        context.clone(),
        receipt.request.bootstrap_policy.clone(),
        receipt.host_incarnation.clone(),
        "proxy".into(),
        Some(ManagedNetworkProof {
            target: context.target,
            guest_ip: receipt.guest_ip,
            network_identity: receipt.network_identity.clone(),
        }),
    )
    .unwrap();
    *host.policy.lock().unwrap() = Some(applied.clone());
    finalize(
        state,
        ManagedBootFinalizeInput {
            key: prepared.request.key.clone(),
            prepared_receipt_id: receipt.receipt_id,
            policy: applied,
            native_version: 1,
            combined_manifest: b"[]".to_vec(),
            combined_manifest_digest: digest(b"[]"),
        },
    )
    .await
    .expect("original A must finalize after catalog changes to B")
}

struct LostManifestReply {
    inner: Arc<dyn engram_core::traits::BlobStorage>,
    target: String,
    lost: AtomicUsize,
}
#[async_trait]
impl engram_core::traits::BlobStorage for LostManifestReply {
    async fn put_streaming(
        &self,
        key: &str,
        body: engram_core::traits::ByteStream,
    ) -> Result<u64, engram_core::BlobError> {
        let size = self.inner.put_streaming(key, body).await?;
        if key == self.target && self.lost.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(engram_core::BlobError::Protocol(
                "reply lost after manifest commit".into(),
            ));
        }
        Ok(size)
    }
    async fn get_streaming(
        &self,
        key: &str,
    ) -> Result<engram_core::traits::ByteStream, engram_core::BlobError> {
        self.inner.get_streaming(key).await
    }
    async fn head(
        &self,
        key: &str,
    ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::BlobError> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), engram_core::BlobError> {
        self.inner.delete(key).await
    }
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, engram_core::BlobError> {
        self.inner.list_prefix(prefix).await
    }
    async fn list_prefix_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<engram_core::traits::ListPage, engram_core::BlobError> {
        self.inner.list_prefix_page(prefix, cursor, limit).await
    }
}

const RPC_TEST_BUDGET: std::time::Duration = std::time::Duration::from_millis(10);
const SCAN_TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

#[tokio::test]
async fn managed_boot_prepare_deadline_allows_next_row_without_replacing_attempt() {
    let (state, meta, host, row) = fixture().await;
    let key = &row.request.key;
    let first = ManagedBootRecord::claimed(row.request.clone());
    let mut second_request = row.request.clone();
    let second_sid = SessionId::from(uuid::Uuid::from_u128(800));
    let second_sandbox = SandboxId::from(uuid::Uuid::from_u128(801));
    second_request.key.creation.session_id = second_sid;
    second_request.key.context.target.session_id = second_sid;
    second_request.key.context.target.sandbox_id = second_sandbox;
    second_request.key.boot_attempt = "boot-two".into();
    second_request.cold.session_id = second_sid;
    second_request.cold.sandbox_id = second_sandbox;
    second_request.cold.boot_attempt = second_request.key.boot_attempt.clone();
    let second_key = second_request.key.clone();
    meta.with_db_mut(|db| {
        let mut session = db.sessions[&key.creation.session_id].clone();
        session.session.id = second_sid;
        session.session.sandbox_id = Some(second_sandbox);
        let mut creation = db.managed_creations[&key.creation.session_id].clone();
        creation.identity = second_request.key.creation.clone();
        db.managed_creations.insert(second_sid, creation);
        db.sessions.insert(second_sid, session);
        db.managed_boots
            .insert(key.creation.session_id, first.clone());
        db.managed_boots.insert(
            second_sid,
            ManagedBootRecord::claimed(second_request.clone()),
        );
    });
    *host.prepare_stall.lock().unwrap() = Some(key.creation.session_id);
    let mut prepared = host.prepared.clone();
    prepared.request = second_request;
    *host.state.lock().unwrap() = Some(ManagedBootHostState::Prepared {
        receipt: Box::new(prepared),
    });
    for _ in 0..2 {
        tokio::time::timeout(
            SCAN_TEST_LIMIT,
            run_once_with_timeout(&state, RPC_TEST_BUDGET),
        )
        .await
        .expect("one host must not block the next row")
        .unwrap();
        assert_eq!(
            serde_json::to_value(meta.read_managed_boot(key).await.unwrap()).unwrap(),
            serde_json::to_value(&first).unwrap()
        );
        assert_eq!(
            meta.read_managed_boot(&second_key).await.unwrap().phase,
            ManagedBootPhase::Prepared
        );
    }
    assert_eq!(host.starts.load(Ordering::SeqCst), 0);
    assert!(gate(&state, key.creation.session_id, true).await.is_err());
}

#[tokio::test]
async fn managed_boot_start_deadline_reconciles_without_another_start() {
    let (state, meta, host, row) = fixture().await;
    let key = &row.request.key;
    host.start_stall.store(true, Ordering::SeqCst);
    let error = tokio::time::timeout(
        SCAN_TEST_LIMIT,
        drive_with_timeout(&state, key, RPC_TEST_BUDGET),
    )
    .await
    .expect("start reply must have a deadline")
    .unwrap_err();
    assert!(matches!(error, ApiError::Unavailable(_)), "{error:?}");
    let issued = meta.read_managed_boot(key).await.unwrap();
    assert_eq!(issued.phase, ManagedBootPhase::StartIssued);
    assert_eq!(issued.request, row.request);
    assert!(gate(&state, key.creation.session_id, true).await.is_err());
    drive_with_timeout(&state, key, RPC_TEST_BUDGET)
        .await
        .unwrap();
    assert_eq!(
        meta.read_managed_boot(key).await.unwrap().phase,
        ManagedBootPhase::Started
    );
    assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    gate(&state, key.creation.session_id, true).await.unwrap();
}

#[tokio::test]
async fn managed_boot_started_read_deadline_interrupts_without_releasing_owner() {
    let (state, meta, host, row) = fixture().await;
    let key = &row.request.key;
    assert!(drive(&state, key).await.is_err());
    drive(&state, key).await.unwrap();
    host.read_stall.store(true, Ordering::SeqCst);
    let error = tokio::time::timeout(
        SCAN_TEST_LIMIT,
        drive_with_timeout(&state, key, RPC_TEST_BUDGET),
    )
    .await
    .expect("started read must have a deadline")
    .unwrap_err();
    assert!(matches!(error, ApiError::Unavailable(_)), "{error:?}");
    let interrupted = meta.read_managed_boot(key).await.unwrap();
    assert_eq!(interrupted.phase, ManagedBootPhase::Interrupted);
    assert_eq!(interrupted.request, row.request);
    assert_eq!(host.starts.load(Ordering::SeqCst), 1);
    let session = meta.get_session(key.creation.session_id).await.unwrap();
    assert_eq!(session.host_id, Some(key.context.target.host_id));
    assert_eq!(session.sandbox_id, Some(key.context.target.sandbox_id));
    assert!(gate(&state, key.creation.session_id, true).await.is_err());
}
