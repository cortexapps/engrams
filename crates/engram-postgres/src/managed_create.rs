//! Atomic immutable authority, in the existing session creation transaction.
use crate::{db_err, PostgresStore};
use engram_core::{
    types::{managed_create::*, mcp_policy::*},
    MetaError,
};
use sqlx::{PgConnection, Row};

fn conflict() -> MetaError {
    MetaError::Conflict("managed creation association changed or unavailable".into())
}

pub(crate) async fn read(
    conn: &mut PgConnection,
    identity: &ManagedCreateIdentity,
) -> Result<ManagedCreation, MetaError> {
    identity.validate()?;
    let row = sqlx::query("SELECT c.* FROM session_managed_creations c JOIN sessions s ON s.id=c.session_id WHERE c.session_id=$1 AND s.status NOT IN ('dead','completed','failed') AND s.mcp_policy_parent_id IS NOT DISTINCT FROM c.parent_session_id")
        .bind(identity.session_id.as_uuid())
        .fetch_optional(conn)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
    let policy = McpPolicyCreation {
        actor_id: row.try_get("actor_id").map_err(db_err)?,
        input_id: row.try_get("input_id").map_err(db_err)?,
        parent_session_id: row
            .try_get::<Option<uuid::Uuid>, _>("parent_session_id")
            .map_err(db_err)?
            .map(Into::into),
        eligibility: match row.try_get::<&str, _>("eligibility").map_err(db_err)? {
            "unknown" => WriteEligibility::Unknown,
            "read_only" => WriteEligibility::ReadOnly,
            "write_eligible" => WriteEligibility::WriteEligible,
            _ => return Err(conflict()),
        },
    };
    let stored = ManagedCreation {
        identity: ManagedCreateIdentity {
            session_id: identity.session_id,
            task_id: row.try_get("task_id").map_err(db_err)?,
            policy,
            input_digest: row.try_get("input_digest").map_err(db_err)?,
        },
        native: FrozenNativeManifest {
            version: u32::try_from(row.try_get::<i32, _>("native_version").map_err(db_err)?)
                .map_err(|_| conflict())?,
            bytes: row.try_get("native_bytes").map_err(db_err)?,
            digest: row.try_get("native_digest").map_err(db_err)?,
        },
    };
    if stored.identity != *identity {
        return Err(conflict());
    }
    stored.native.validate()?;
    Ok(stored)
}

/// true: new reservation; false: exact retry. Deferred FK commits with the session row.
pub(crate) async fn reserve(
    conn: &mut PgConnection,
    creation: &ManagedCreation,
) -> Result<bool, MetaError> {
    let i = &creation.identity;
    let n = sqlx::query("INSERT INTO session_managed_creations (session_id, task_id, input_id, actor_id, parent_session_id, eligibility, input_digest, native_version, native_bytes, native_digest) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT DO NOTHING")
        .bind(i.session_id.as_uuid()).bind(&i.task_id).bind(&i.policy.input_id).bind(&i.policy.actor_id)
        .bind(i.policy.parent_session_id.map(|p| p.as_uuid())).bind(i.policy.eligibility.as_str())
        .bind(&i.input_digest).bind(i32::try_from(creation.native.version).map_err(|_| conflict())?)
        .bind(&creation.native.bytes).bind(&creation.native.digest)
        .execute(&mut *conn).await.map_err(db_err)?.rows_affected();
    if n == 1 {
        return Ok(true);
    }
    if read(conn, i).await? != *creation {
        return Err(conflict());
    }
    // An exact lost-response retry never restarts a session or changes placement.
    let held: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions s JOIN session_managed_creations c ON c.session_id=s.id WHERE s.id=$1 AND c.state='held' AND s.status='queued' AND s.host_id IS NULL AND s.sandbox_id IS NULL AND s.binding_epoch=0)")
        .bind(i.session_id.as_uuid()).fetch_one(&mut *conn).await.map_err(db_err)?;
    if !held {
        return Err(conflict());
    }
    Ok(false)
}

impl PostgresStore {
    pub(crate) async fn managed_creation(
        &self,
        identity: &ManagedCreateIdentity,
    ) -> Result<ManagedCreation, MetaError> {
        read(&mut *self.pool.acquire().await.map_err(db_err)?, identity).await
    }
}
