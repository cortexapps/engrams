//! Short transaction lock shared with the authenticated operator publisher.
use engram_core::{types::managed_boot::ManagedBootKey, MetaError};
use sqlx::{PgConnection, Row};

/// Acquire this before session/ancestor locks. The publisher uses FOR UPDATE,
/// closes admission and invalidates readiness before it releases its lock.
/// No host I/O is permitted while either lock is held.
pub(crate) async fn require(
    conn: &mut PgConnection,
    key: &ManagedBootKey,
) -> Result<(), MetaError> {
    let row = sqlx::query("SELECT published_generation,published_identity,admission_open FROM managed_operator_generation_gate WHERE singleton FOR SHARE")
        .fetch_optional(conn).await.map_err(crate::db_err)?;
    let Some(row) = row else {
        return Err(closed());
    };
    let generation: Option<i64> = row.try_get("published_generation").map_err(crate::db_err)?;
    let identity: Option<String> = row.try_get("published_identity").map_err(crate::db_err)?;
    let open: bool = row.try_get("admission_open").map_err(crate::db_err)?;
    if !open
        || generation != i64::try_from(key.context.operator_generation).ok()
        || identity.as_ref() != Some(&key.context.operator_identity)
    {
        return Err(closed());
    }
    Ok(())
}
fn closed() -> MetaError {
    MetaError::Conflict("managed operator generation is closed".into())
}
