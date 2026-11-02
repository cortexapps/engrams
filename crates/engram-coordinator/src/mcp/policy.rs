//! Trusted policy service. It is never served on application gRPC.
use super::control::Control;
use crate::state::SharedState;
use engram_core::{types::mcp_policy::*, SessionId};
use engram_protocol::{app, mcp as wire};
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
#[tonic::async_trait]
impl SessionPolicyService for PolicyControl {
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
        let answer = self
            .control
            .authority
            .manager
            .meta
            .validate_mcp_dispatch_fence(&ctx)
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
}
#[cfg(test)]
mod tests {
    use super::*;
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
