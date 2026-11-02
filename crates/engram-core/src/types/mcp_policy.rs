//! Conservative server authorization, independent of guest mode reports.
use super::event::PersistedEvent;
use crate::{MetaError, SessionId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteEligibility {
    Unknown,
    ReadOnly,
    WriteEligible,
}
impl WriteEligibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ReadOnly => "read_only",
            Self::WriteEligible => "write_eligible",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WritePolicyRef {
    pub session_id: SessionId,
    pub policy_revision: i64,
    pub binding_epoch: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpPolicyCreation {
    pub actor_id: String,
    pub input_id: String,
    pub parent_session_id: Option<SessionId>,
    pub eligibility: WriteEligibility,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredWritePolicy {
    pub revision: i64,
    pub eligibility: WriteEligibility,
    pub initialized: bool,
    pub cause: String,
    pub accepted_input_id: String,
    pub actor_id: Option<String>,
    pub prompt_id: Option<String>,
    pub plan_tool_call_id: Option<String>,
    pub plan_request_digest: Option<String>,
    /// Requests before the accepted restriction cannot approve that restriction.
    pub event_floor: i64,
}
impl StoredWritePolicy {
    pub fn initial(input: &McpPolicyCreation) -> Self {
        Self {
            revision: 1,
            eligibility: input.eligibility,
            initialized: input.eligibility != WriteEligibility::Unknown,
            cause: "initial".into(),
            accepted_input_id: input.input_id.clone(),
            actor_id: Some(input.actor_id.clone()),
            prompt_id: None,
            plan_tool_call_id: None,
            plan_request_digest: None,
            event_floor: 0,
        }
    }
    pub fn restrict(
        previous: Option<&Self>,
        input_id: &str,
        event_floor: i64,
    ) -> Result<Self, MetaError> {
        Ok(Self {
            revision: previous
                .map_or(0, |p| p.revision)
                .checked_add(1)
                .ok_or_else(|| conflict("policy revision exhausted"))?,
            eligibility: WriteEligibility::ReadOnly,
            initialized: previous.is_some_and(|p| p.initialized),
            cause: "plan_accept".into(),
            accepted_input_id: input_id.into(),
            actor_id: None,
            prompt_id: Some(input_id.into()),
            plan_tool_call_id: None,
            plan_request_digest: None,
            event_floor,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedWritePolicy {
    #[serde(rename = "ref")]
    pub reference: WritePolicyRef,
    pub write_eligibility: WriteEligibility,
    pub cause: String,
    pub accepted_input_id: String,
    pub authenticated_actor_id: Option<String>,
    pub prompt_id: Option<String>,
    pub plan_tool_call_id: Option<String>,
    pub plan_request_digest: Option<String>,
    pub ancestor_policy_refs: Vec<WritePolicyRef>,
    pub current: bool,
    pub initialized: bool,
}
/// Internal row view. The parent comes from persistence, never caller-supplied ancestry.
#[derive(Clone, Debug)]
pub struct PolicyNode {
    pub id: SessionId,
    pub binding_epoch: i64,
    pub parent: Option<SessionId>,
    pub policy: Option<StoredWritePolicy>,
    pub terminal: bool,
    pub active: bool,
}
impl PolicyNode {
    pub fn reference(&self) -> WritePolicyRef {
        WritePolicyRef {
            session_id: self.id,
            policy_revision: self.policy.as_ref().map_or(0, |p| p.revision),
            binding_epoch: self.binding_epoch,
        }
    }
}
pub fn compose_policy(nodes: &[PolicyNode]) -> Result<TrustedWritePolicy, MetaError> {
    let leaf = nodes.first().ok_or(MetaError::NotFound)?;
    let mut eligibility = WriteEligibility::WriteEligible;
    let mut current = true;
    for node in nodes {
        let e = node
            .policy
            .as_ref()
            .map_or(WriteEligibility::Unknown, |p| p.eligibility);
        current &= !node.terminal;
        eligibility = match (eligibility, e) {
            (WriteEligibility::Unknown, _) | (_, WriteEligibility::Unknown) => {
                WriteEligibility::Unknown
            }
            (WriteEligibility::ReadOnly, _) | (_, WriteEligibility::ReadOnly) => {
                WriteEligibility::ReadOnly
            }
            _ => WriteEligibility::WriteEligible,
        };
    }
    if !current {
        eligibility = WriteEligibility::Unknown;
    }
    let p = leaf.policy.as_ref();
    Ok(TrustedWritePolicy {
        reference: leaf.reference(),
        write_eligibility: eligibility,
        cause: p.map_or("unknown", |p| p.cause.as_str()).into(),
        accepted_input_id: p.map_or("", |p| p.accepted_input_id.as_str()).into(),
        authenticated_actor_id: p.and_then(|p| p.actor_id.clone()),
        prompt_id: p.and_then(|p| p.prompt_id.clone()),
        plan_tool_call_id: p.and_then(|p| p.plan_tool_call_id.clone()),
        plan_request_digest: p.and_then(|p| p.plan_request_digest.clone()),
        ancestor_policy_refs: nodes.iter().skip(1).map(PolicyNode::reference).collect(),
        current,
        initialized: p.is_some_and(|p| p.initialized),
    })
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReview {
    pub policy_ref: WritePolicyRef,
    pub tool_call_id: String,
    pub run_id: String,
    pub event_idx: i64,
    pub request_digest: String,
    pub args_json: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanAcceptance {
    pub review: PlanReview,
    pub actor_id: String,
    pub result_json: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPolicyContext {
    pub session_id: SessionId,
    pub run_id: String,
    pub tool_call_id: String,
    pub accepted_policy_ref: WritePolicyRef,
    pub accepted_write_eligibility: WriteEligibility,
    pub ancestor_policy_refs: Vec<WritePolicyRef>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchFence {
    Current,
    Stale,
    Unknown,
}
pub fn conflict(message: &str) -> MetaError {
    MetaError::Conflict(message.into())
}
/// Canonical envelope preserves the exact submitted argument bytes.
fn request_digest(event: &PersistedEvent) -> String {
    let p = &event.payload;
    let bytes=serde_json::to_vec(&serde_json::json!({"run_id":p["run_id"],"tool_call_id":p["tool_call_id"],"name":p["name"],"args_json":p["args_json"]})).expect("JSON value serializes");
    hex::encode(Sha256::digest(bytes))
}
pub fn pending_request<'a>(
    events: &'a [PersistedEvent],
    call: &str,
) -> Result<&'a PersistedEvent, MetaError> {
    let relevant: Vec<_> = events
        .iter()
        .filter(|e| {
            e.payload["tool_call_id"].as_str() == Some(call)
                && matches!(
                    e.kind.as_str(),
                    "tool_call_requested" | "tool_result_submitted" | "tool_call_completed"
                )
        })
        .collect();
    if relevant.len() >= 1024 {
        return Err(conflict("request history limit exceeded"));
    }
    let first = relevant
        .iter()
        .find(|e| e.kind == "tool_call_requested")
        .ok_or_else(|| conflict("pending request missing"))?;
    if first.rewound_at.is_some()
        || relevant.iter().any(|e| {
            matches!(
                e.kind.as_str(),
                "tool_result_submitted" | "tool_call_completed"
            ) || (e.kind == "tool_call_requested" && request_digest(e) != request_digest(first))
        })
    {
        return Err(conflict("pending request changed"));
    }
    Ok(first)
}
pub fn plan_review(
    node: &PolicyNode,
    events: &[PersistedEvent],
    call: &str,
) -> Result<PlanReview, MetaError> {
    let policy = node
        .policy
        .as_ref()
        .ok_or_else(|| conflict("policy unknown"))?;
    if node.terminal || !policy.initialized || policy.eligibility != WriteEligibility::ReadOnly {
        return Err(conflict("known restrictive policy required"));
    }
    let event = pending_request(events, call)?;
    if event.idx < policy.event_floor || event.payload["name"].as_str() != Some("exit_plan_mode") {
        return Err(conflict("current plan request required"));
    }
    let run_id = event.payload["run_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| conflict("run required"))?;
    let args_json = event.payload["args_json"]
        .as_str()
        .ok_or_else(|| conflict("plan arguments required"))?;
    Ok(PlanReview {
        policy_ref: node.reference(),
        tool_call_id: call.into(),
        run_id: run_id.into(),
        event_idx: event.idx,
        request_digest: request_digest(event),
        args_json: args_json.into(),
    })
}
pub fn accept_plan(
    node: &PolicyNode,
    events: &[PersistedEvent],
    input: &PlanAcceptance,
) -> Result<StoredWritePolicy, MetaError> {
    if input.actor_id.is_empty()
        || input.actor_id.len() > 256
        || plan_review(node, events, &input.review.tool_call_id)? != input.review
    {
        return Err(conflict("plan review changed"));
    }
    let result: serde_json::Value =
        serde_json::from_str(&input.result_json).map_err(|_| conflict("invalid plan result"))?;
    if result["decision"] != "approve" {
        return Err(conflict("only human approval can relax policy"));
    }
    let mut p = node
        .policy
        .clone()
        .ok_or_else(|| conflict("policy unknown"))?;
    p.revision = p
        .revision
        .checked_add(1)
        .ok_or_else(|| conflict("policy revision exhausted"))?;
    p.eligibility = WriteEligibility::WriteEligible;
    p.cause = "owner_admin_plan_exit".into();
    p.accepted_input_id = super::outbox::tool_result_outbox_id(node.id, &input.review.tool_call_id);
    p.actor_id = Some(input.actor_id.clone());
    p.plan_tool_call_id = Some(input.review.tool_call_id.clone());
    p.plan_request_digest = Some(input.review.request_digest.clone());
    Ok(p)
}
pub fn dispatch_fence(
    nodes: &[PolicyNode],
    events: &[PersistedEvent],
    ctx: &RequestPolicyContext,
) -> Result<DispatchFence, MetaError> {
    let p = compose_policy(nodes)?;
    if !nodes[0].active {
        return Ok(DispatchFence::Stale);
    }
    if ctx.accepted_write_eligibility != WriteEligibility::WriteEligible
        || p.write_eligibility != WriteEligibility::WriteEligible
    {
        return Ok(DispatchFence::Unknown);
    }
    if ctx.session_id != ctx.accepted_policy_ref.session_id
        || p.reference != ctx.accepted_policy_ref
        || p.ancestor_policy_refs != ctx.ancestor_policy_refs
    {
        return Ok(DispatchFence::Stale);
    }
    let request = match pending_request(events, &ctx.tool_call_id) {
        Ok(r) => r,
        Err(_) => return Ok(DispatchFence::Stale),
    };
    let last_run = events.iter().rev().find(|e| {
        e.rewound_at.is_none()
            && matches!(
                e.kind.as_str(),
                "run_started" | "run_completed" | "run_interrupted"
            )
    });
    if ctx.run_id.is_empty()
        || request.payload["run_id"].as_str() != Some(&ctx.run_id)
        || last_run.is_none_or(|e| {
            e.kind != "run_started" || e.payload["run_id"].as_str() != Some(&ctx.run_id)
        })
    {
        return Ok(DispatchFence::Stale);
    }
    Ok(DispatchFence::Current)
}
