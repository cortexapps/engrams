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
}
#[async_trait]
impl HostClient for LossyHost {
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
            operator_identity: digest(b"operator"),
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
    let creation = sim.read_managed_creation(&key.creation).await.unwrap();
    meta.reserve_and_persist_create(
        engram_core::traits::SessionCreateWriteSet {
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
