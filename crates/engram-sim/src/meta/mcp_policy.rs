use super::{SimDb, SimMetadataStore};
use chrono::{DateTime, Utc};
use engram_core::{
    types::{event::PersistedEvent, mcp_policy::*, outbox::*},
    MetaError, SessionId,
};
use std::collections::BTreeSet;
pub(super) fn nodes(db: &SimDb, id: SessionId) -> Result<Vec<PolicyNode>, MetaError> {
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    let mut next = Some(id);
    while let Some(id) = next {
        if result.len() >= 32 || !seen.insert(id) {
            return Err(conflict("policy ancestry unavailable"));
        }
        let row = db.sessions.get(&id).ok_or(MetaError::NotFound)?;
        result.push(PolicyNode {
            id,
            binding_epoch: row.binding_epoch,
            parent: row.mcp_policy_parent_id,
            policy: row.mcp_write_policy.clone(),
            terminal: row.session.status.is_terminal(),
            active: row.session.status == engram_core::types::session::SessionState::Active,
        });
        next = row.mcp_policy_parent_id;
    }
    Ok(result)
}
pub(super) fn append(
    db: &mut SimDb,
    id: SessionId,
    kind: &str,
    payload: serde_json::Value,
    now: DateTime<Utc>,
) -> Result<i64, MetaError> {
    let row = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
    let idx = row.next_event_idx;
    row.next_event_idx += 1;
    row.session.last_event_at = Some(now);
    row.updated_at = now;
    let recovery_epoch = row.recovery_epoch;
    db.session_events
        .entry(id)
        .or_default()
        .push(PersistedEvent {
            idx,
            kind: kind.into(),
            payload,
            created_at: now,
            recovery_epoch,
            rewound_at: None,
        });
    Ok(idx)
}
impl SimMetadataStore {
    pub(super) fn policy_read(&self, id: SessionId) -> Result<TrustedWritePolicy, MetaError> {
        self.gate()?;
        compose_policy(&nodes(&self.db.lock(), id)?)
    }
    pub(super) fn policy_review(&self, id: SessionId, call: &str) -> Result<PlanReview, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let ns = nodes(&db, id)?;
        plan_review(
            &ns[0],
            db.session_events.get(&id).map_or(&[], Vec::as_slice),
            call,
        )
    }
    pub(super) fn policy_fence(
        &self,
        ctx: &RequestPolicyContext,
    ) -> Result<DispatchFence, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        dispatch_fence(
            &nodes(&db, ctx.session_id)?,
            db.session_events
                .get(&ctx.session_id)
                .map_or(&[], Vec::as_slice),
            ctx,
        )
    }
    pub(super) fn policy_accept(
        &self,
        input: &PlanAcceptance,
    ) -> Result<TrustedWritePolicy, MetaError> {
        self.gate()?;
        let id = input.review.policy_ref.session_id;
        let now = self.now();
        let mut db = self.db.lock();
        let mut ns = nodes(&db, id)?;
        let outbox_id = tool_result_outbox_id(id, &input.review.tool_call_id);
        let payload = serde_json::json!({"tool_call_id":input.review.tool_call_id,"result_json":input.result_json,"mcp_policy_acceptance":input});
        if let Some(row) = db.outbox.get(&outbox_id) {
            if row.session_id != id || row.kind != OutboxKind::ToolResult || row.payload != payload
            {
                return Err(conflict("plan result already accepted"));
            }
            return compose_policy(&ns);
        }
        let updated = accept_plan(
            &ns[0],
            db.session_events.get(&id).map_or(&[], Vec::as_slice),
            input,
        )?;
        db.sessions
            .get_mut(&id)
            .ok_or(MetaError::NotFound)?
            .mcp_write_policy = Some(updated.clone());
        db.outbox.insert(
            outbox_id.clone(),
            OutboxRow {
                prompt_id: outbox_id,
                session_id: id,
                kind: OutboxKind::ToolResult,
                payload,
                created_at: now,
                attempts: 0,
                not_before: now,
                delivered_at: None,
                acked_at: None,
            },
        );
        let idx = append(
            &mut db,
            id,
            "tool_result_submitted",
            serde_json::json!({"tool_call_id":input.review.tool_call_id,"result_json":input.result_json,"at":now}),
            now,
        )?;
        ns[0].policy = Some(updated);
        let p = compose_policy(&ns)?;
        drop(db);
        self.notify(
            "session_events",
            serde_json::json!({"session_id":id,"idx":idx}).to_string(),
        );
        self.notify("session_outbox", id.to_string());
        Ok(p)
    }
}
