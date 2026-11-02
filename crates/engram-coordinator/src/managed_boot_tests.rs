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
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
struct LossyHost {
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
        Err(SandboxError::Unavailable("reply lost after spawn".into()))
    }
    async fn read_managed_boot(
        &self,
        _: ManagedBootKey,
    ) -> Result<ManagedBootHostState, SandboxError> {
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
    let host = Arc::new(LossyHost {
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
    let blob = Arc::new(MemBlobStorage::new());
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
