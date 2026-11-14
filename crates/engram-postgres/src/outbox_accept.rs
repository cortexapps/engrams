use crate::db_err;
use engram_core::{MetaError, SessionId};
use sqlx::Row;
pub(crate) async fn accept(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    now: chrono::DateTime<chrono::Utc>,
    session_id: SessionId,
    events: &[(String, serde_json::Value)],
    outbox: &engram_core::types::outbox::OutboxRow,
) -> Result<Option<Vec<i64>>, MetaError> {
    if outbox.session_id != session_id {
        return Err(MetaError::Serialization(
            "event/outbox session ids do not match".into(),
        ));
    }
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE")
        .bind(session_id.as_uuid())
        .fetch_optional(&mut **tx)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
    crate::managed_boot::require_tool_result_target(tx, outbox).await?;
    let inserted = sqlx::query(
        r#"
            INSERT INTO session_outbox
                (prompt_id, session_id, kind, payload, created_at, not_before)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (prompt_id) DO NOTHING
            RETURNING prompt_id
            "#,
    )
    .bind(&outbox.prompt_id)
    .bind(session_id.as_uuid())
    .bind(outbox.kind.as_str())
    .bind(&outbox.payload)
    .bind(outbox.created_at)
    .bind(outbox.not_before)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_err)?
    .is_some();

    if !inserted {
        // A retry of the SAME command is the designed no-op; a
        // prompt_id claimed by a DIFFERENT command — including the
        // same kind with different text/mode (review finding on
        // #993: payload must be part of the identity, as it is in
        // `append_session_event_and_outbox`) — is a Conflict, never
        // a silent drop.
        let existing = sqlx::query(
            "SELECT session_id, kind, payload FROM session_outbox WHERE prompt_id = $1",
        )
        .bind(&outbox.prompt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db_err)?
        .ok_or_else(|| {
            MetaError::Conflict(format!(
                "outbox id {} disappeared during accept",
                outbox.prompt_id
            ))
        })?;
        let existing_session: uuid::Uuid = existing.try_get("session_id").map_err(db_err)?;
        let existing_kind: String = existing.try_get("kind").map_err(db_err)?;
        let existing_payload: serde_json::Value = existing.try_get("payload").map_err(db_err)?;
        if existing_session != session_id.as_uuid()
            || existing_kind != outbox.kind.as_str()
            || existing_payload != outbox.payload
        {
            return Err(MetaError::Conflict(format!(
                "outbox id {} belongs to another command",
                outbox.prompt_id
            )));
        }
        return Ok(None);
    }

    if outbox.kind == engram_core::types::outbox::OutboxKind::Prompt
        && outbox.payload["mode"] == "plan"
    {
        let row = sqlx::query("SELECT mcp_write_policy,next_event_idx FROM sessions WHERE id=$1")
            .bind(session_id.as_uuid())
            .fetch_one(&mut **tx)
            .await
            .map_err(db_err)?;
        let old: Option<serde_json::Value> = row.try_get("mcp_write_policy").map_err(db_err)?;
        let old = old
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| MetaError::Conflict("policy unavailable".into()))?;
        let policy = engram_core::types::mcp_policy::StoredWritePolicy::restrict(
            old.as_ref(),
            &outbox.prompt_id,
            row.try_get("next_event_idx").map_err(db_err)?,
        )?;
        sqlx::query("UPDATE sessions SET mcp_write_policy=$2 WHERE id=$1")
            .bind(session_id.as_uuid())
            .bind(
                serde_json::to_value(policy)
                    .map_err(|_| MetaError::Conflict("policy encode failed".into()))?,
            )
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
    }
    let mut idxs = Vec::with_capacity(events.len());
    for (kind, payload) in events {
        let event = sqlx::query(
                r#"
                WITH next AS (
                    UPDATE sessions
                       SET next_event_idx = next_event_idx + 1,
                           updated_at = $4,
                           last_event_at = $4
                     WHERE id = $1
                 RETURNING next_event_idx - 1 AS allocated_idx, recovery_epoch
                ),
                inserted AS (
                    INSERT INTO session_events (session_id, idx, kind, payload, recovery_epoch, created_at)
                    SELECT $1, allocated_idx, $2, $3, recovery_epoch, $4 FROM next
                    RETURNING idx
                )
                SELECT idx FROM inserted
                "#,
            )
            .bind(session_id.as_uuid())
            .bind(kind)
            .bind(payload)
            .bind(now)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db_err)?
            .ok_or(MetaError::NotFound)?;
        idxs.push(event.try_get::<i64, _>("idx").map_err(db_err)?);
    }

    // Notifications fire at commit: replicas see the events and the
    // outbox row together, so delivery can never observe the row
    // without its echo (the ADR 0052 echo-before-run_started
    // ordering, now transactional instead of sequenced).
    for idx in &idxs {
        sqlx::query("SELECT pg_notify('session_events', $1)")
            .bind(
                serde_json::json!({ "session_id": session_id.to_string(), "idx": idx }).to_string(),
            )
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
    }
    sqlx::query("SELECT pg_notify('session_outbox', $1)")
        .bind(session_id.to_string())
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    Ok(Some(idxs))
}
