//! Coordinator recovery through the actual apply/read functions; no guest is started.
use super::*;
use async_trait::async_trait;
use engram_core::traits::{Clock, Entropy, HostClient, MetadataStore, SessionFence};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::managed_deny::*;
use engram_core::types::sandbox::{
    AgentSpec, ExecRequest as HostExecRequest, ExecStream, SandboxProbe, SandboxSpec,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::{host::*, SessionSpec};
use engram_core::{SandboxError, SandboxId};
use engram_sim::{ManualClock, MemBlobStorage, SimEntropy, SimMetadataStore};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct PolicyHost {
    applied: Mutex<Option<ManagedAppliedState>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
#[async_trait]
impl HostClient for PolicyHost {
    async fn read_managed_policy(
        &self,
        _: ManagedPolicyTarget,
    ) -> Result<Option<ManagedAppliedState>, SandboxError> {
        Ok(self.applied.lock().unwrap().clone())
    }
    async fn apply_managed_policy(
        &self,
        request: ManagedPolicyApply,
    ) -> Result<ManagedAppliedState, SandboxError> {
        let mut current = self.applied.lock().unwrap();
        assert_eq!(
            *current, request.expected,
            "exact applied receipt must remain the CAS input"
        );
        let ack = ManagedAppliedState::new(
            request.context,
            request.egress.managed_deny.unwrap(),
            "fixture-host".into(),
            "fixture-proxy".into(),
            None,
        )
        .unwrap();
        *current = Some(ack.clone());
        Ok(ack)
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
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        Some("10.200.0.42".parse().unwrap())
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

async fn retained_recovery(meta: Arc<dyn MetadataStore>, clock: Arc<ManualClock>) {
    let entropy = Arc::new(SimEntropy::seeded(111));
    let sid = meta
        .create_session(SessionSpec {
            image: "test.invalid/policy:latest".into(),
            mode: Default::default(),
        })
        .await
        .unwrap();
    let host_id = engram_core::HostId::from(entropy.uuid());
    let sandbox = SandboxId::from(entropy.uuid());
    meta.upsert_host(host_record(host_id, "fixture-host", clock.now_utc()))
        .await
        .unwrap();
    meta.assign_session_host(sid, Some(host_id)).await.unwrap();
    let binding_epoch = meta.transition_session_created(sid, sandbox).await.unwrap();
    let target = ManagedPolicyTarget {
        session_id: sid,
        sandbox_id: sandbox,
        host_id,
        binding_epoch,
    };
    let context = ManagedPolicyContext {
        target: target.clone(),
        operator_generation: 1,
        operator_identity: format!("sha256:{}", "a".repeat(64)),
        binding_hash: None,
    };
    let one = ManagedDenyPolicy::new(1, ["a.example".into()]).unwrap();
    meta.retain_managed_deny(&target, &one).await.unwrap();
    let initial_ack = ManagedAppliedState::new(
        context.clone(),
        one.clone(),
        "fixture-host".into(),
        "fixture-proxy".into(),
        None,
    )
    .unwrap();
    let host = Arc::new(PolicyHost {
        applied: Mutex::new(Some(initial_ack.clone())),
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
            [0u8; 32], "test:v1",
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
    let state = Arc::new(crate::AppState::new(
        crate::CoordinatorConfig::default(),
        services,
    ));
    // Explicit wrapper + absent policy is authoritative absence for a fresh row.
    let fresh = meta
        .create_session(SessionSpec {
            image: "test.invalid/fresh:latest".into(),
            mode: Default::default(),
        })
        .await
        .unwrap();
    meta.assign_session_host(fresh, Some(host_id))
        .await
        .unwrap();
    let fresh_sandbox = SandboxId::from(state.services.entropy.uuid());
    let fresh_epoch = meta
        .transition_session_created(fresh, fresh_sandbox)
        .await
        .unwrap();
    let fresh_target = ManagedPolicyTarget {
        session_id: fresh,
        sandbox_id: fresh_sandbox,
        host_id,
        binding_epoch: fresh_epoch,
    };
    *host.applied.lock().unwrap() = None;
    let empty = read_managed_policy(&state, &fresh_target)
        .await
        .unwrap()
        .into_inner();
    empty.validate_for(&fresh_target).unwrap();
    assert!(empty.applied.is_none());
    assert!(empty.retained_desired.unwrap().policy.is_none());
    *host.applied.lock().unwrap() = Some(initial_ack.clone());
    let three = ManagedDenyPolicy::new(3, ["a.example".into(), "orphan.example".into()]).unwrap();
    let pending = tokio::spawn({
        let state = state.clone();
        let context = context.clone();
        let three = three.clone();
        let expected = initial_ack.clone();
        async move { apply_managed_policy(&state, context, three, Some(expected)).await }
    });
    tokio::time::timeout(Duration::from_secs(3), host.entered.notified())
        .await
        .unwrap();
    // The real apply path is suspended at guest-IP host I/O after desired commit.
    // A concurrent writer on the same row must complete while that I/O is held.
    tokio::time::timeout(
        Duration::from_secs(3),
        meta.retain_managed_deny(&target, &three),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*host.applied.lock().unwrap(), Some(initial_ack.clone()));
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    for applied in [Some(initial_ack.clone()), None] {
        *host.applied.lock().unwrap() = applied.clone();
        let response = read_managed_policy(&state, &target)
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            response
                .applied
                .map(ManagedAppliedState::try_from)
                .transpose()
                .unwrap(),
            applied
        );
        let retained = response
            .retained_desired
            .expect("complete read must expose the committed orphan union");
        assert_eq!(
            ManagedPolicyTarget::try_from(retained.target.unwrap()).unwrap(),
            target
        );
        assert_eq!(
            ManagedDenyPolicy::try_from(retained.policy.unwrap()).unwrap(),
            three
        );
    }
    // A fresh consumer chooses max(retained, applied) + 1 and preserves both unions.
    let read = read_managed_policy(&state, &target)
        .await
        .unwrap()
        .into_inner();
    let retained: ManagedDenyPolicy = read
        .retained_desired
        .unwrap()
        .policy
        .unwrap()
        .try_into()
        .unwrap();
    let expected = read
        .applied
        .map(ManagedAppliedState::try_from)
        .transpose()
        .unwrap();
    let revision = retained
        .revision
        .max(expected.as_ref().map_or(0, |ack| ack.policy.revision))
        + 1;
    let four = ManagedDenyPolicy::new(
        revision,
        retained.hosts.into_iter().chain(["next.example".into()]),
    )
    .unwrap();
    host.release.add_permits(1);
    let retry = apply_managed_policy(&state, context, four.clone(), expected)
        .await
        .unwrap()
        .into_inner();
    let ack = ManagedAppliedState::try_from(retry.applied.unwrap()).unwrap();
    assert_eq!(ack.policy, four);
    assert_eq!(ack.policy.revision, 4);
    let response = read_managed_policy(&state, &target)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        ManagedDenyPolicy::try_from(response.retained_desired.unwrap().policy.unwrap()).unwrap(),
        four
    );
}

#[tokio::test]
async fn retained_desired_recovers_cancelled_apply_sim() {
    let clock = ManualClock::new();
    let meta = SimMetadataStore::new(clock.clone(), Arc::new(SimEntropy::seeded(222)));
    retained_recovery(meta, clock).await;
}
#[tokio::test]
async fn retained_desired_recovers_cancelled_apply_pg() {
    let Some(db) = engram_testkit::pg::fresh_db().await else {
        return;
    };
    let clock = ManualClock::new();
    let meta = Arc::new(
        db.store
            .clone()
            .with_clock(clock.clone())
            .with_entropy(Arc::new(SimEntropy::seeded(222))),
    );
    retained_recovery(meta, clock).await;
}
fn host_record(
    id: engram_core::HostId,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> HostRecord {
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
        host_addr: None,
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

#[tokio::test]
async fn managed_retry_denies_requeued_nonzero_epoch() {
    use engram_core::types::{
        managed_create::digest, runtime_spec::RuntimeSpec, BindingDisposition, SessionState,
    };
    let meta = SimMetadataStore::new(ManualClock::new(), Arc::new(SimEntropy::seeded(333)));
    let id = SessionId::from(uuid::Uuid::from_u128(333));
    let policy = McpPolicyCreation {
        actor_id: "owner-a".into(),
        input_id: "create-a".into(),
        parent_session_id: None,
        eligibility: WriteEligibility::ReadOnly,
    };
    let frozen = ManagedCreation {
        identity: ManagedCreateIdentity {
            session_id: id,
            task_id: "task-a".into(),
            policy: policy.clone(),
            input_digest: digest(b"original"),
        },
        native: FrozenNativeManifest {
            version: 1,
            bytes: b"[]".to_vec(),
            digest: digest(b"[]"),
        },
    };
    assert!(!managed_creation_retry(meta.as_ref(), &frozen)
        .await
        .unwrap());
    let ws = engram_core::traits::SessionCreateWriteSet {
        managed_boot_selection: None,
        managed_initial_prompt: None,
        managed_creation: Some(frozen.clone()),
        mcp_policy: Some(policy),
        nbd_slot_need: 1,
        session_id: id,
        spec: SessionSpec {
            image: "test.invalid/managed:latest".into(),
            mode: Default::default(),
        },
        mem_budget_mib: 512,
        cpu_budget_vcpus: 1,
        sealed_secrets: None,
        capabilities: Vec::new(),
        integration_policy_json: None,
        runtime_spec: RuntimeSpec::new(Vec::new(), Some("claude".into()), None, Vec::new()),
        oauth_binding: None,
    };
    engram_testkit::operator::publish_empty(meta.as_ref()).await;
    meta.reserve_and_persist_create(ws.clone(), &[], 0)
        .await
        .unwrap();
    assert!(managed_creation_retry(meta.as_ref(), &frozen)
        .await
        .unwrap());
    meta.transition_session(
        id,
        SessionState::Pending,
        BindingDisposition::RequireUnbound,
    )
    .await
    .unwrap();
    assert_eq!(
        meta.transition_session_created(id, SandboxId::from(uuid::Uuid::from_u128(334)))
            .await
            .unwrap(),
        1
    );
    meta.transition_session(id, SessionState::Idle, BindingDisposition::Detach)
        .await
        .unwrap();
    meta.transition_session(id, SessionState::Queued, BindingDisposition::RequireUnbound)
        .await
        .unwrap();
    let session = meta.get_session(id).await.unwrap();
    assert_eq!(session.status, SessionState::Queued);
    assert!(session.host_id.is_none() && session.sandbox_id.is_none());
    assert_eq!(
        meta.read_managed_creation(&frozen.identity).await.unwrap(),
        frozen
    );
    assert!(meta.reserve_and_persist_create(ws, &[], 0).await.is_err());
    assert_eq!(
        managed_creation_retry(meta.as_ref(), &frozen)
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(
        {
            let empty =
                SimMetadataStore::new(ManualClock::new(), Arc::new(SimEntropy::seeded(334)));
            require_managed_create_admission(empty.as_ref())
                .await
                .unwrap_err()
                .code()
        },
        tonic::Code::Unavailable
    );
}
