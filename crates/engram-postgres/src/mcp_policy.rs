use super::{db_err, PostgresStore};
use engram_core::{
    types::{
        event::PersistedEvent,
        mcp_policy::*,
        outbox::{tool_result_outbox_id, OutboxKind},
    },
    MetaError, SessionId,
};
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeSet;

pub(super) async fn nodes(
    tx: &mut Transaction<'_, Postgres>,
    id: SessionId,
) -> Result<Vec<PolicyNode>, MetaError> {
    // Parent links never change. Read the chain, then lock every row in ID order.
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    let mut next = Some(id);
    while let Some(id) = next {
        if chain.len() >= 32 || !seen.insert(id) {
            return Err(conflict("policy ancestry unavailable"));
        }
        let row = sqlx::query("SELECT mcp_policy_parent_id FROM sessions WHERE id=$1")
            .bind(id.as_uuid())
            .fetch_optional(&mut **tx)
            .await
            .map_err(db_err)?
            .ok_or(MetaError::NotFound)?;
        next = row
            .try_get::<Option<uuid::Uuid>, _>("mcp_policy_parent_id")
            .map_err(db_err)?
            .map(SessionId);
        chain.push(id);
    }
    let ids: Vec<_> = seen.iter().map(|id| id.as_uuid()).collect();
    let rows=sqlx::query("SELECT id, binding_epoch, mcp_policy_parent_id, mcp_write_policy, status FROM sessions WHERE id=ANY($1) ORDER BY id FOR UPDATE")
        .bind(ids).fetch_all(&mut **tx).await.map_err(db_err)?;
    if rows.len() != chain.len() {
        return Err(conflict("policy ancestry unavailable"));
    }
    let mut result = Vec::new();
    for id in chain {
        let row = rows
            .iter()
            .find(|r| r.get::<uuid::Uuid, _>("id") == id.as_uuid())
            .ok_or(MetaError::NotFound)?;
        let raw: Option<serde_json::Value> = row.try_get("mcp_write_policy").map_err(db_err)?;
        let policy = raw
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| conflict("policy unavailable"))?;
        let status: String = row.try_get("status").map_err(db_err)?;
        result.push(PolicyNode {
            id,
            binding_epoch: row.try_get("binding_epoch").map_err(db_err)?,
            parent: row
                .try_get::<Option<uuid::Uuid>, _>("mcp_policy_parent_id")
                .map_err(db_err)?
                .map(SessionId),
            policy,
            terminal: matches!(status.as_str(), "dead" | "completed" | "failed"),
            active: status == "active",
        });
    }
    Ok(result)
}
async fn events(
    tx: &mut Transaction<'_, Postgres>,
    id: SessionId,
    call: &str,
) -> Result<Vec<PersistedEvent>, MetaError> {
    // At most one run event accompanies call history. A truncated history always
    // contains at least 1024 call events, which the shared validator rejects.
    let rows=sqlx::query("SELECT idx,kind,payload,created_at,recovery_epoch::bigint AS recovery_epoch,rewound_at FROM session_events WHERE session_id=$1 AND ((payload->>'tool_call_id'=$2 AND kind IN ('tool_call_requested','tool_result_submitted','tool_call_completed','tool_call_cancel_requested')) OR idx=(SELECT max(idx) FROM session_events WHERE session_id=$1 AND rewound_at IS NULL AND kind IN ('run_started','run_completed','run_interrupted'))) ORDER BY idx LIMIT 1025")
        .bind(id.as_uuid()).bind(call).fetch_all(&mut **tx).await.map_err(db_err)?;
    rows.into_iter()
        .map(|r| {
            Ok(PersistedEvent {
                idx: r.try_get("idx").map_err(db_err)?,
                kind: r.try_get("kind").map_err(db_err)?,
                payload: r.try_get("payload").map_err(db_err)?,
                created_at: r.try_get("created_at").map_err(db_err)?,
                recovery_epoch: r.try_get("recovery_epoch").map_err(db_err)?,
                rewound_at: r.try_get("rewound_at").map_err(db_err)?,
            })
        })
        .collect()
}
impl PostgresStore {
    pub(super) async fn policy_read(&self, id: SessionId) -> Result<TrustedWritePolicy, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let p = compose_policy(&nodes(&mut tx, id).await?)?;
        tx.commit().await.map_err(db_err)?;
        Ok(p)
    }
    pub(super) async fn policy_review(
        &self,
        id: SessionId,
        call: &str,
    ) -> Result<PlanReview, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let ns = nodes(&mut tx, id).await?;
        let p = plan_review(&ns[0], &events(&mut tx, id, call).await?, call)?;
        tx.commit().await.map_err(db_err)?;
        Ok(p)
    }
    pub(super) async fn policy_fence(
        &self,
        ctx: &RequestPolicyContext,
        expected: &ExecutionRequest,
        write: bool,
    ) -> Result<DispatchFence, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let ns = nodes(&mut tx, ctx.session_id).await?;
        let check = if write {
            dispatch_fence
        } else {
            execution_identity_fence
        };
        let answer = check(
            &ns,
            &events(&mut tx, ctx.session_id, &ctx.tool_call_id).await?,
            ctx,
            expected,
        )?;
        tx.commit().await.map_err(db_err)?;
        Ok(answer)
    }
    pub(super) async fn policy_accept(
        &self,
        input: &PlanAcceptance,
    ) -> Result<TrustedWritePolicy, MetaError> {
        let id = input.review.policy_ref.session_id;
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let mut ns = nodes(&mut tx, id).await?;
        let outbox_id = tool_result_outbox_id(id, &input.review.tool_call_id);
        let payload = serde_json::json!({"tool_call_id":input.review.tool_call_id,"result_json":input.result_json,"mcp_policy_acceptance":input});
        if let Some(row) = sqlx::query(
            "SELECT payload FROM session_outbox WHERE prompt_id=$1 AND session_id=$2 AND kind=$3",
        )
        .bind(&outbox_id)
        .bind(id.as_uuid())
        .bind(OutboxKind::ToolResult.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        {
            if row
                .try_get::<serde_json::Value, _>("payload")
                .map_err(db_err)?
                != payload
            {
                return Err(conflict("plan result already accepted"));
            }
            let p = compose_policy(&ns)?;
            tx.commit().await.map_err(db_err)?;
            return Ok(p);
        }
        let updated = accept_plan(
            &ns[0],
            &events(&mut tx, id, &input.review.tool_call_id).await?,
            input,
        )?;
        sqlx::query("INSERT INTO session_outbox(prompt_id,session_id,kind,payload,created_at,not_before) VALUES($1,$2,$3,$4,$5,$5)").bind(&outbox_id).bind(id.as_uuid()).bind(OutboxKind::ToolResult.as_str()).bind(payload).bind(now).execute(&mut *tx).await.map_err(db_err)?;
        let idx:i64=sqlx::query_scalar("UPDATE sessions SET mcp_write_policy=$2,next_event_idx=next_event_idx+1,last_event_at=$3,updated_at=$3 WHERE id=$1 RETURNING next_event_idx-1").bind(id.as_uuid()).bind(serde_json::to_value(&updated).map_err(|_|conflict("policy encode failed"))?).bind(now).fetch_one(&mut *tx).await.map_err(db_err)?;
        let event = serde_json::json!({"tool_call_id":input.review.tool_call_id,"result_json":input.result_json,"at":now});
        sqlx::query("INSERT INTO session_events(session_id,idx,kind,payload,recovery_epoch,created_at) SELECT id,$2,'tool_result_submitted',$3,recovery_epoch,$4 FROM sessions WHERE id=$1").bind(id.as_uuid()).bind(idx).bind(event).bind(now).execute(&mut *tx).await.map_err(db_err)?;
        sqlx::query("SELECT pg_notify('session_events',$1),pg_notify('session_outbox',$2)")
            .bind(serde_json::json!({"session_id":id,"idx":idx}).to_string())
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        ns[0].policy = Some(updated);
        let p = compose_policy(&ns)?;
        tx.commit().await.map_err(db_err)?;
        Ok(p)
    }
}
