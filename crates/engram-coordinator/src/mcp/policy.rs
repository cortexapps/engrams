//! Trusted policy service. It is never served on application gRPC.
use super::control::Control;
use crate::state::SharedState;
use engram_core::types::managed_create::{
    FrozenNativeManifest, ManagedCreateIdentity, ManagedCreation,
};
use engram_core::{types::mcp_policy::*, SessionId};
use engram_protocol::{app, mcp as wire};
#[cfg(test)]
use prost::Message;
use tonic::{Request, Response, Status};
use wire::session_policy_service_server::SessionPolicyService;
#[derive(Clone)]
pub(super) struct PolicyControl {
    pub control: Control,
    pub state: Option<SharedState>,
}
fn id(value: &str) -> Result<SessionId, Status> {
    value
        .parse()
        .map_err(|_| Status::invalid_argument("invalid session"))
}
fn err(_: engram_core::MetaError) -> Status {
    Status::failed_precondition("session policy changed or unavailable")
}
fn required<T>(value: Option<T>) -> Result<T, Status> {
    value.ok_or_else(|| Status::invalid_argument("required policy field missing"))
}
fn policy_ref(r: WritePolicyRef) -> app::McpWritePolicyRef {
    app::McpWritePolicyRef {
        session_id: r.session_id.to_string(),
        policy_revision: r.policy_revision,
        binding_epoch: r.binding_epoch,
    }
}
fn from_ref(r: app::McpWritePolicyRef) -> Result<WritePolicyRef, Status> {
    if r.policy_revision < 0 || r.binding_epoch < 0 {
        return Err(Status::invalid_argument("invalid policy reference"));
    }
    Ok(WritePolicyRef {
        session_id: id(&r.session_id)?,
        policy_revision: r.policy_revision,
        binding_epoch: r.binding_epoch,
    })
}
fn policy(p: TrustedWritePolicy) -> app::McpTrustedWritePolicy {
    app::McpTrustedWritePolicy {
        r#ref: Some(policy_ref(p.reference)),
        write_eligibility: p.write_eligibility.as_str().into(),
        cause: p.cause,
        accepted_input_id: p.accepted_input_id,
        authenticated_actor_id: p.authenticated_actor_id,
        prompt_id: p.prompt_id,
        plan_tool_call_id: p.plan_tool_call_id,
        plan_request_digest: p.plan_request_digest,
        ancestor_policy_refs: p.ancestor_policy_refs.into_iter().map(policy_ref).collect(),
        current: p.current,
        initialized: p.initialized,
    }
}
fn review(r: PlanReview) -> app::McpPlanReview {
    app::McpPlanReview {
        policy_ref: Some(policy_ref(r.policy_ref)),
        tool_call_id: r.tool_call_id,
        run_id: r.run_id,
        event_idx: r.event_idx,
        request_digest: r.request_digest,
        args_json: r.args_json,
    }
}
fn from_review(r: app::McpPlanReview) -> Result<PlanReview, Status> {
    Ok(PlanReview {
        policy_ref: from_ref(required(r.policy_ref)?)?,
        tool_call_id: r.tool_call_id,
        run_id: r.run_id,
        event_idx: r.event_idx,
        request_digest: r.request_digest,
        args_json: r.args_json,
    })
}
fn eligibility(s: &str) -> Result<WriteEligibility, Status> {
    match s {
        "unknown" => Ok(WriteEligibility::Unknown),
        "read_only" => Ok(WriteEligibility::ReadOnly),
        "write_eligible" => Ok(WriteEligibility::WriteEligible),
        _ => Err(Status::invalid_argument("invalid write eligibility")),
    }
}
fn creation(request: &wire::CreatePolicySessionRequest) -> Result<McpPolicyCreation, Status> {
    if request.actor_id.is_empty()
        || request.actor_id.len() > 256
        || request.input_id.is_empty()
        || request.input_id.len() > 256
    {
        return Err(Status::invalid_argument("creation identity required"));
    }
    let create = request
        .create
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("creation required"))?;
    let eligibility = match create.harness_mode.as_deref() {
        None | Some("" | "default") => WriteEligibility::WriteEligible,
        Some("plan") => WriteEligibility::ReadOnly,
        _ => return Err(Status::invalid_argument("unsupported policy mode")),
    };
    Ok(McpPolicyCreation {
        actor_id: request.actor_id.clone(),
        input_id: request.input_id.clone(),
        parent_session_id: request.parent_session_id.as_deref().map(id).transpose()?,
        eligibility,
    })
}
fn managed_identity(r: wire::ManagedCreateIdentity) -> Result<ManagedCreateIdentity, Status> {
    let identity = ManagedCreateIdentity {
        session_id: id(&r.session_id)?,
        task_id: r.task_id,
        input_digest: r.input_digest,
        policy: McpPolicyCreation {
            actor_id: r.actor_id,
            input_id: r.input_id,
            parent_session_id: r.parent_session_id.as_deref().map(id).transpose()?,
            eligibility: eligibility(&r.write_eligibility)?,
        },
    };
    identity.validate().map_err(err)?;
    Ok(identity)
}
fn identity_wire(i: ManagedCreateIdentity) -> wire::ManagedCreateIdentity {
    wire::ManagedCreateIdentity {
        session_id: i.session_id.to_string(),
        task_id: i.task_id,
        input_id: i.policy.input_id,
        actor_id: i.policy.actor_id,
        parent_session_id: i.policy.parent_session_id.map(|p| p.to_string()),
        write_eligibility: i.policy.eligibility.as_str().into(),
        input_digest: i.input_digest,
    }
}
fn managed_creation(
    r: &wire::ReserveManagedCreateRequest,
) -> Result<(ManagedCreation, app::CreateSessionRequest), Status> {
    if r.protocol != wire::PROTOCOL
        || r.creation_request.len() > 3 * 1024 * 1024
        || r.input_digest != wire::managed_create_input_digest(r)
    {
        return Err(Status::invalid_argument("invalid managed creation input"));
    }
    let policy_request = r
        .decode_creation()
        .map_err(|_| Status::invalid_argument("invalid creation request"))?;
    if policy_request.protocol != wire::PROTOCOL {
        return Err(Status::invalid_argument("unsupported control protocol"));
    }
    let policy = creation(&policy_request)?;
    let create = required(policy_request.create)?;
    let session_id = id(create
        .requested_session_id
        .as_deref()
        .ok_or_else(|| Status::invalid_argument("reserved session required"))?)?;
    if !matches!(create.mode.as_str(), "" | "agent")
        || !matches!(create.harness.as_deref(), Some("claude" | "codex"))
    {
        return Err(Status::invalid_argument(
            "managed creation requires a supported agent harness",
        ));
    }
    let bytes = create
        .harness_env
        .get("ENGRAM_TOOLS")
        .ok_or_else(|| Status::invalid_argument("native manifest required"))?
        .as_bytes()
        .to_vec();
    let frozen = ManagedCreation {
        identity: ManagedCreateIdentity {
            session_id,
            task_id: r.task_id.clone(),
            policy,
            input_digest: r.input_digest.clone(),
        },
        native: FrozenNativeManifest {
            version: r.native_version,
            bytes,
            digest: r.native_digest.clone(),
        },
    };
    frozen.identity.validate().map_err(err)?;
    frozen.native.validate().map_err(err)?;
    Ok((frozen, create))
}

/// A missing creation may enter preparation; an existing creation must still be held.
async fn managed_creation_retry(
    meta: &dyn engram_core::traits::MetadataStore,
    frozen: &ManagedCreation,
) -> Result<bool, Status> {
    match meta.read_held_managed_creation(&frozen.identity).await {
        Ok(existing) if existing == *frozen => Ok(true),
        Ok(_) => Err(Status::failed_precondition("creation input changed")),
        Err(engram_core::MetaError::NotFound) => Ok(false),
        Err(e) => Err(err(e)),
    }
}

/// Read-only preflight. The create transaction repeats this under the generation lock.
async fn require_managed_create_admission(
    meta: &dyn engram_core::traits::MetadataStore,
) -> Result<(), Status> {
    let publication = meta.read_operator_publication().await.map_err(err)?;
    if !publication.admission_open
        || publication.pending.is_some()
        || publication.published.is_none()
    {
        return Err(Status::unavailable(
            "managed creation admission unavailable",
        ));
    }
    Ok(())
}

#[tonic::async_trait]
impl SessionPolicyService for PolicyControl {
    async fn read_published_operator(
        &self,
        req: Request<wire::ReadPublishedOperatorRequest>,
    ) -> Result<Response<wire::ReadPublishedOperatorResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        let publication = self
            .control
            .authority
            .manager
            .meta
            .read_operator_publication()
            .await
            .map_err(err)?;
        if !publication.admission_open
            || publication.pending.is_some()
            || publication.published.is_none()
        {
            return Err(Status::unavailable("operator publication unavailable"));
        }
        Ok(Response::new(wire::ReadPublishedOperatorResponse {
            protocol: wire::PROTOCOL,
            publication_json: serde_json::to_vec(&publication)
                .map_err(|_| Status::internal("publication encoding failed"))?,
        }))
    }

    async fn read_managed_creation_state(
        &self,
        req: Request<wire::ReadManagedCreationStateRequest>,
    ) -> Result<Response<wire::ReadManagedCreationStateResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let r = req.into_inner();
        let q = bridge_query(r.identity, r.native_version, r.native_digest)?;
        let state = self
            .control
            .authority
            .manager
            .meta
            .read_managed_creation_state(&q)
            .await
            .map_err(bridge_err)?;
        Ok(Response::new(wire::ReadManagedCreationStateResponse {
            protocol: wire::PROTOCOL,
            state_json: engram_protocol::managed_boot::encode(&state)
                .map_err(|_| Status::internal("state encoding failed"))?,
        }))
    }
    async fn prepare_managed_boot(
        &self,
        req: Request<wire::PrepareManagedBootRequest>,
    ) -> Result<Response<wire::PrepareManagedBootResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let r = req.into_inner();
        let q = bridge_query(r.identity, r.native_version, r.native_digest)?;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| Status::unavailable("managed boot unavailable"))?;
        let view = crate::managed_bridge::prepare(state, q)
            .await
            .map_err(bridge_api_err)?;
        Ok(Response::new(wire::PrepareManagedBootResponse {
            protocol: wire::PROTOCOL,
            state_json: engram_protocol::managed_boot::encode(&view)
                .map_err(|_| Status::internal("state encoding failed"))?,
        }))
    }
    async fn recover_managed_boot(
        &self,
        req: Request<wire::RecoverManagedBootRequest>,
    ) -> Result<Response<wire::RecoverManagedBootResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let r = req.into_inner();
        let query = bridge_query(r.identity, r.native_version, r.native_digest)?;
        let source = engram_protocol::managed_boot::decode(&r.expected_source_key_json)
            .map_err(|_| Status::invalid_argument("invalid recovery source"))?;
        let input = engram_core::types::managed_bridge::ManagedRecoveryInput {
            query,
            expected_source_key: source,
            recovery_id: r.recovery_id,
        };
        input.validate().map_err(bridge_err)?;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| Status::unavailable("managed boot unavailable"))?;
        let view = crate::managed_bridge::recover(state, input)
            .await
            .map_err(bridge_api_err)?;
        Ok(Response::new(wire::RecoverManagedBootResponse {
            protocol: wire::PROTOCOL,
            state_json: engram_protocol::managed_boot::encode(&view)
                .map_err(|_| Status::internal("state encoding failed"))?,
        }))
    }
    async fn read_managed_boot(
        &self,
        req: Request<wire::ReadManagedBootRequest>,
    ) -> Result<Response<wire::ReadManagedBootResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        if req.get_ref().protocol != wire::PROTOCOL {
            return Err(Status::invalid_argument("unsupported protocol"));
        }
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let identity = managed_identity(required(req.into_inner().identity)?)?;
        let row = self
            .control
            .authority
            .manager
            .meta
            .get_managed_boot(&identity)
            .await
            .map_err(err)?;
        Ok(Response::new(wire::ReadManagedBootResponse {
            protocol: wire::PROTOCOL,
            boot_json: boot_json(row)?,
        }))
    }
    async fn finalize_managed_boot(
        &self,
        req: Request<wire::FinalizeManagedBootRequest>,
    ) -> Result<Response<wire::FinalizeManagedBootResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        if req.get_ref().protocol != wire::PROTOCOL {
            return Err(Status::invalid_argument("unsupported protocol"));
        }
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let input = engram_protocol::managed_boot::decode(&req.into_inner().input_json)
            .map_err(|_| Status::invalid_argument("invalid managed finalization"))?;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| Status::unavailable("managed boot unavailable"))?;
        let row = crate::managed_boot::finalize(state, input)
            .await
            .map_err(|_| {
                Status::failed_precondition("managed boot authority changed or unavailable")
            })?;
        Ok(Response::new(wire::FinalizeManagedBootResponse {
            protocol: wire::PROTOCOL,
            boot_json: boot_json(Some(row))?,
        }))
    }
    async fn reserve_managed_create(
        &self,
        req: Request<wire::ReserveManagedCreateRequest>,
    ) -> Result<Response<wire::ReserveManagedCreateResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let (frozen, create) = managed_creation(req.get_ref())?;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| Status::unavailable("creation unavailable"))?;
        let identity = frozen.identity.clone();
        // The exact retry is independent of current image/profile resolution.
        if !managed_creation_retry(self.control.authority.manager.meta.as_ref(), &frozen).await? {
            require_managed_create_admission(self.control.authority.manager.meta.as_ref()).await?;
            let identity_env = create.harness_env.clone().into_iter().collect();
            let mut api = crate::grpc_app::convert::create_request_from_proto(create)
                .map_err(|_| Status::invalid_argument("invalid creation request"))?;
            api.mcp_creation = Some(identity.policy.clone());
            api.managed_creation = Some(frozen);
            crate::api::sessions::create_session_core(state, identity_env, api, None)
                .await
                .map_err(|_| Status::failed_precondition("session reservation unavailable"))?;
        }
        Ok(Response::new(wire::ReserveManagedCreateResponse {
            protocol: wire::PROTOCOL,
            identity: Some(identity_wire(identity)),
            state: "held".into(),
        }))
    }
    async fn read_native_manifest(
        &self,
        req: Request<wire::ReadNativeManifestRequest>,
    ) -> Result<Response<wire::ReadNativeManifestResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let identity = managed_identity(required(req.into_inner().identity)?)?;
        let stored = self
            .control
            .authority
            .manager
            .meta
            .read_managed_creation(&identity)
            .await
            .map_err(err)?;
        Ok(Response::new(wire::ReadNativeManifestResponse {
            protocol: wire::PROTOCOL,
            identity: Some(identity_wire(stored.identity)),
            native_version: stored.native.version,
            native_bytes: stored.native.bytes,
            native_digest: stored.native.digest,
        }))
    }

    async fn read_managed_policy(
        &self,
        req: Request<wire::ReadManagedPolicyRequest>,
    ) -> Result<Response<wire::ReadManagedPolicyResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let target: engram_core::types::managed_deny::ManagedPolicyTarget =
            required(req.into_inner().target)?
                .try_into()
                .map_err(Status::invalid_argument)?;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| Status::unavailable("managed policy unavailable"))?;
        read_managed_policy(state, &target).await
    }
    async fn apply_managed_policy(
        &self,
        req: Request<wire::ApplyManagedPolicyRequest>,
    ) -> Result<Response<wire::ApplyManagedPolicyResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let r = req.into_inner();
        let context: engram_core::types::managed_deny::ManagedPolicyContext = required(r.context)?
            .try_into()
            .map_err(Status::invalid_argument)?;
        let policy: engram_core::types::managed_deny::ManagedDenyPolicy = required(r.policy)?
            .try_into()
            .map_err(Status::invalid_argument)?;
        let expected = r
            .expected
            .map(TryInto::try_into)
            .transpose()
            .map_err(Status::invalid_argument)?;
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| Status::unavailable("managed policy unavailable"))?;
        apply_managed_policy(state, context, policy, expected).await
    }
    async fn create_policy_session(
        &self,
        req: Request<wire::CreatePolicySessionRequest>,
    ) -> Result<Response<wire::CreatePolicySessionResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let context = creation(req.get_ref())?;
        let r = required(req.into_inner().create)?;
        let identity_env = r.harness_env.clone().into_iter().collect();
        let mut api = crate::grpc_app::convert::create_request_from_proto(r)
            .map_err(|_| Status::invalid_argument("invalid creation request"))?;
        api.mcp_creation = Some(context);
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| Status::unavailable("creation unavailable"))?;
        let result = crate::api::sessions::create_session_core(state, identity_env, api, None)
            .await
            .map_err(|_| Status::failed_precondition("session creation unavailable"))?;
        Ok(Response::new(wire::CreatePolicySessionResponse {
            session: Some(app::CreateSessionResponse {
                session_id: result.session_id.to_string(),
                status: result.status.into(),
                image_version: result.image_version,
                kind: result.kind.into(),
            }),
        }))
    }
    async fn read_write_policy(
        &self,
        req: Request<wire::ReadWritePolicyRequest>,
    ) -> Result<Response<wire::ReadWritePolicyResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let r = req.into_inner();
        let sid = id(&r.session_id)?;
        let meta = &self.control.authority.manager.meta;
        let p = meta.read_mcp_write_policy(sid).await.map_err(err)?;
        let reason = if !p.initialized {
            "External writes are blocked because this session has no trusted initial policy. Start a new supported session."
        } else {
            match p.write_eligibility {
            WriteEligibility::Unknown=>"External writes are blocked because this session or an ancestor has unknown policy.",
            WriteEligibility::ReadOnly=>"External writes are blocked by this session or an ancestor. The owner or an administrator must approve the applicable current plan.",
            WriteEligibility::WriteEligible=>"External writes still require tool-specific authorization.",
        }
        };
        let reviewed = match r.plan_tool_call_id {
            Some(call) => {
                let reviewed = meta.read_mcp_plan_review(sid, &call).await.map_err(err)?;
                if reviewed.policy_ref != p.reference {
                    return Err(Status::failed_precondition(
                        "session policy changed; reload the plan",
                    ));
                }
                Some(review(reviewed))
            }
            None => None,
        };
        // Review includes its own revision. A restriction between these reads cannot relax policy.
        Ok(Response::new(wire::ReadWritePolicyResponse {
            state: Some(app::GetMcpWritePolicyResponse {
                policy: Some(policy(p)),
                plan_review: reviewed,
                reason: reason.into(),
                allowed_actions: Vec::new(),
            }),
        }))
    }
    async fn accept_plan_exit(
        &self,
        req: Request<wire::AcceptPlanExitRequest>,
    ) -> Result<Response<wire::AcceptPlanExitResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let r = req.into_inner();
        let input = PlanAcceptance {
            review: from_review(required(r.expected)?)?,
            actor_id: r.actor_id,
            result_json: r.result_json,
        };
        let sid = input.review.policy_ref.session_id;
        let p = self
            .control
            .authority
            .manager
            .meta
            .accept_mcp_plan_exit(&input)
            .await
            .map_err(err)?;
        if let Some(state) = &self.state {
            crate::outbox_delivery::enqueue_deliver_op(state, sid).await;
            state.outbox_wake.notify_one();
        }
        Ok(Response::new(wire::AcceptPlanExitResponse {
            policy: Some(policy(p)),
        }))
    }
    async fn validate_dispatch_fence(
        &self,
        req: Request<wire::ValidateDispatchFenceRequest>,
    ) -> Result<Response<wire::ValidateDispatchFenceResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let r = req.into_inner();
        let ctx = RequestPolicyContext {
            session_id: id(&r.session_id)?,
            run_id: r.run_id,
            tool_call_id: r.tool_call_id,
            accepted_policy_ref: from_ref(required(r.accepted_policy_ref)?)?,
            accepted_write_eligibility: eligibility(&r.accepted_write_eligibility)?,
            ancestor_policy_refs: r
                .ancestor_policy_refs
                .into_iter()
                .map(from_ref)
                .collect::<Result<_, _>>()?,
        };
        if self
            .control
            .authority
            .manager
            .meta
            .managed_boot_open(ctx.session_id)
            .await
            .map_err(err)?
            == Some(false)
        {
            return Err(Status::failed_precondition("managed execution gate closed"));
        }
        let answer = self
            .control
            .authority
            .manager
            .meta
            .validate_mcp_dispatch_fence(
                &ctx,
                &ExecutionRequest {
                    model_tool_name: r.model_tool_name,
                    original_arguments: r.original_arguments,
                },
            )
            .await
            .map_err(err)?;
        Ok(Response::new(wire::ValidateDispatchFenceResponse {
            protocol: wire::PROTOCOL,
            status: match answer {
                DispatchFence::Current => "current",
                DispatchFence::Stale => "stale",
                DispatchFence::Unknown => "unknown",
            }
            .into(),
        }))
    }
    async fn validate_execution_identity(
        &self,
        req: Request<wire::ValidateExecutionIdentityRequest>,
    ) -> Result<Response<wire::ValidateExecutionIdentityResponse>, Status> {
        let _admission = self.control.check(&req, req.get_ref().protocol)?;
        self.control
            .authority
            .enabled()
            .map_err(|_| Status::unavailable("policy unavailable"))?;
        let r = req.into_inner();
        let ctx = RequestPolicyContext {
            session_id: id(&r.session_id)?,
            run_id: r.run_id,
            tool_call_id: r.tool_call_id,
            accepted_policy_ref: from_ref(required(r.accepted_policy_ref)?)?,
            accepted_write_eligibility: eligibility(&r.accepted_write_eligibility)?,
            ancestor_policy_refs: r
                .ancestor_policy_refs
                .into_iter()
                .map(from_ref)
                .collect::<Result<_, _>>()?,
        };
        if self
            .control
            .authority
            .manager
            .meta
            .managed_boot_open(ctx.session_id)
            .await
            .map_err(err)?
            == Some(false)
        {
            return Err(Status::failed_precondition("managed execution gate closed"));
        }
        let answer = self
            .control
            .authority
            .manager
            .meta
            .validate_mcp_execution_identity(
                &ctx,
                &ExecutionRequest {
                    model_tool_name: r.model_tool_name,
                    original_arguments: r.original_arguments,
                },
            )
            .await
            .map_err(err)?;
        Ok(Response::new(wire::ValidateExecutionIdentityResponse {
            protocol: wire::PROTOCOL,
            status: match answer {
                DispatchFence::Current => "current",
                DispatchFence::Stale => "stale",
                DispatchFence::Unknown => "unknown",
            }
            .into(),
        }))
    }
}

async fn read_managed_policy(
    state: &SharedState,
    target: &engram_core::types::managed_deny::ManagedPolicyTarget,
) -> Result<Response<wire::ReadManagedPolicyResponse>, Status> {
    check_managed_target(state, target).await?;
    let applied = state
        .services
        .host
        .read_managed_policy(target.clone())
        .await
        .map_err(|_| Status::failed_precondition("host policy unavailable"))?;
    // Read after host I/O so a concurrent desired commit is retained. No SQL
    // transaction crosses host I/O, and persisted bytes never become an ACK.
    let retained = crate::api::sessions::load_session_policy(state, target.session_id)
        .await
        .map_err(|_| Status::failed_precondition("retained policy unavailable"))?
        .and_then(|policy| policy.managed_deny);
    check_managed_target(state, target).await?;
    let response = wire::ReadManagedPolicyResponse {
        protocol: wire::PROTOCOL,
        applied: applied.map(Into::into),
        retained_desired: Some(wire::ManagedRetainedDesired {
            target: Some(target.clone().into()),
            policy: retained.map(Into::into),
        }),
    };
    response
        .validate_for(target)
        .map_err(Status::failed_precondition)?;
    Ok(Response::new(response))
}

async fn apply_managed_policy(
    state: &SharedState,
    context: engram_core::types::managed_deny::ManagedPolicyContext,
    policy: engram_core::types::managed_deny::ManagedDenyPolicy,
    expected: Option<engram_core::types::managed_deny::ManagedAppliedState>,
) -> Result<Response<wire::ApplyManagedPolicyResponse>, Status> {
    let target = &context.target;
    check_managed_target(state, target).await?;
    // The desired union commits before any host call. A lost ACK cannot discard it on migration.
    state
        .services
        .meta
        .retain_managed_deny(target, &policy)
        .await
        .map_err(err)?;
    let session = state
        .services
        .meta
        .get_session(target.session_id)
        .await
        .map_err(err)?;
    let egress = crate::api::sessions::build_resume_egress_policy(
        state,
        target.session_id,
        target.sandbox_id,
        &session.image.to_string(),
    )
    .await
    .map_err(|_| Status::failed_precondition("managed egress source unavailable"))?
    .ok_or_else(|| Status::failed_precondition("managed guest identity unavailable"))?;
    if !egress.tunnels.is_empty() {
        return Err(Status::failed_precondition(
            "managed policy does not support tunnels",
        ));
    }
    if egress.managed_deny.as_ref() != Some(&policy) {
        return Err(Status::failed_precondition("desired policy changed"));
    }
    check_managed_target(state, target).await?;
    let applied = state
        .services
        .host
        .apply_managed_policy(engram_core::types::managed_deny::ManagedPolicyApply {
            expected,
            context: context.clone(),
            egress,
        })
        .await
        .map_err(|_| Status::failed_precondition("host policy unavailable"))?;
    check_managed_target(state, target).await?;
    applied.validate().map_err(Status::failed_precondition)?;
    if applied.context != context || applied.policy != policy {
        return Err(Status::failed_precondition("host acknowledgement mismatch"));
    }
    Ok(Response::new(wire::ApplyManagedPolicyResponse {
        protocol: wire::PROTOCOL,
        applied: Some(applied.into()),
    }))
}

async fn check_managed_target(
    state: &SharedState,
    target: &engram_core::types::managed_deny::ManagedPolicyTarget,
) -> Result<(), Status> {
    if state
        .services
        .meta
        .managed_policy_target(target.session_id)
        .await
        .map_err(err)?
        != *target
    {
        return Err(Status::failed_precondition("physical placement changed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_creation_requires_exact_native_input_and_reserved_identity() {
        let sid = "00000000-0000-0000-0000-000000000001";
        let make = |native: Option<&str>| {
            let mut create = app::CreateSessionRequest {
                requested_session_id: Some(sid.into()),
                harness: Some("codex".into()),
                ..Default::default()
            };
            if let Some(native) = native {
                create
                    .harness_env
                    .insert("ENGRAM_TOOLS".into(), native.into());
            }
            let policy = wire::CreatePolicySessionRequest {
                protocol: 1,
                actor_id: "owner".into(),
                input_id: "create-a".into(),
                create: Some(create),
                ..Default::default()
            };
            let mut r = wire::ReserveManagedCreateRequest {
                protocol: 1,
                creation_request: policy.encode_to_vec(),
                task_id: "task-a".into(),
                native_version: 1,
                native_digest: engram_core::types::managed_create::digest(
                    native.unwrap_or("").as_bytes(),
                ),
                input_digest: String::new(),
            };
            r.input_digest = wire::managed_create_input_digest(&r);
            r
        };
        let original = make(Some(" [ ] "));
        let (frozen, _) = managed_creation(&original).unwrap();
        assert_eq!(frozen.native.bytes, b" [ ] ");
        assert_eq!(frozen.identity.session_id.to_string(), sid);
        assert!(managed_creation(&make(None)).is_err());
        let mut wrong = original.clone();
        wrong.task_id = "task-b".into();
        assert!(managed_creation(&wrong).is_err());
        let mut wrong = original.clone();
        wrong.native_version = 2;
        wrong.input_digest = wire::managed_create_input_digest(&wrong);
        assert!(managed_creation(&wrong).is_err());
        let mut wrong = original;
        wrong.creation_request.push(0);
        assert!(managed_creation(&wrong).is_err());
    }

    #[test]
    fn creation_policy_comes_only_from_explicit_supported_creation_input() {
        let mut r = wire::CreatePolicySessionRequest {
            protocol: 1,
            actor_id: "owner-a".into(),
            input_id: "create-a".into(),
            create: Some(app::CreateSessionRequest::default()),
            ..Default::default()
        };
        assert_eq!(
            creation(&r).unwrap().eligibility,
            WriteEligibility::WriteEligible
        );
        r.create.as_mut().unwrap().harness_mode = Some("plan".into());
        assert_eq!(
            creation(&r).unwrap().eligibility,
            WriteEligibility::ReadOnly
        );
        r.create.as_mut().unwrap().harness_mode = Some("custom".into());
        assert!(creation(&r).is_err());
        r.actor_id.clear();
        assert!(creation(&r).is_err());
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod recovery_tests;

fn boot_json(
    row: Option<engram_core::types::managed_boot::ManagedBootRecord>,
) -> Result<Option<Vec<u8>>, Status> {
    row.map(|mut r| {
        r.sealed_agent = None;
        engram_protocol::managed_boot::encode(&r)
            .map_err(|_| Status::internal("managed boot encoding failed"))
    })
    .transpose()
}

fn bridge_query(
    identity: Option<wire::ManagedCreateIdentity>,
    native_version: u32,
    native_digest: String,
) -> Result<engram_core::types::managed_bridge::ManagedCreationQuery, Status> {
    let q = engram_core::types::managed_bridge::ManagedCreationQuery {
        identity: managed_identity(required(identity)?)?,
        native_version,
        native_digest,
    };
    q.validate().map_err(bridge_err)?;
    Ok(q)
}
fn bridge_err(e: engram_core::MetaError) -> Status {
    match e {
        engram_core::MetaError::Conflict(_) | engram_core::MetaError::NotFound => {
            Status::failed_precondition("managed creation authority changed")
        }
        _ => Status::unavailable("managed creation state unavailable"),
    }
}
fn bridge_api_err(e: crate::error::ApiError) -> Status {
    match e {
        crate::error::ApiError::Conflict(_) | crate::error::ApiError::NotFound(_) => {
            Status::failed_precondition("managed creation authority changed")
        }
        _ => Status::unavailable("managed creation state unavailable"),
    }
}
