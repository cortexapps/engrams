//! Publication transactions are short. None performs host I/O.
use crate::{db_err, PostgresStore};
use engram_core::{
    types::{managed_boot::ManagedBootRecord, managed_operator::*},
    MetaError, SessionId,
};
use sqlx::{PgConnection, Row};

const IDS: &str = "SELECT session_id FROM session_managed_creations UNION SELECT session_id FROM session_managed_boots UNION SELECT session_id FROM session_integration_policy WHERE policy_json::jsonb->'managed_deny' IS NOT NULL AND policy_json::jsonb->'managed_deny' <> 'null'::jsonb";
async fn gate(conn: &mut PgConnection) -> Result<(), MetaError> {
    sqlx::query(
        "SELECT singleton FROM managed_operator_generation_gate WHERE singleton FOR UPDATE",
    )
    .fetch_one(conn)
    .await
    .map_err(db_err)?;
    Ok(())
}
async fn state(conn: &mut PgConnection) -> Result<OperatorPublication, MetaError> {
    let row=sqlx::query("SELECT published_generation,published_identity,admission_open FROM managed_operator_generation_gate WHERE singleton").fetch_one(&mut *conn).await.map_err(db_err)?;
    let id: Option<i64> = row.try_get("published_generation").map_err(db_err)?;
    let mut result = OperatorPublication {
        admission_open: row.try_get("admission_open").map_err(db_err)?,
        ..Default::default()
    };
    for row in sqlx::query("SELECT record,published FROM managed_operator_publications WHERE NOT published OR generation=$1").bind(id).fetch_all(conn).await.map_err(db_err)? {
        let value:serde_json::Value=row.try_get("record").map_err(db_err)?;
        let generation:OperatorGeneration=serde_json::from_value(value).map_err(|_| conflict())?;
        if row.try_get::<bool,_>("published").map_err(db_err)? { result.published=Some(generation); } else { result.pending=Some(generation); }
    }
    let identity: Option<String> = row.try_get("published_identity").map_err(db_err)?;
    // A seeded or old gate is never a configured published projection.
    if result.published.as_ref().map(|p| p.generation) != id.and_then(|v| u64::try_from(v).ok())
        || result.published.as_ref().is_none_or(|p| {
            Some(&p.identity) != identity.as_ref()
                || p.candidate.identity().ok().as_ref() != Some(&p.identity)
        })
        || result.pending.is_some()
    {
        result.admission_open = false;
    }
    Ok(result)
}
async fn snapshot(
    conn: &mut PgConnection,
    sid: SessionId,
    hosts: Vec<String>,
    write: bool,
    first: bool,
) -> Result<OperatorParticipant, MetaError> {
    let row=sqlx::query("SELECT s.status,s.host_id,s.sandbox_id,s.binding_epoch, c.state='held' AS held,b.record,p.policy_json FROM sessions s LEFT JOIN session_managed_creations c ON c.session_id=s.id LEFT JOIN session_managed_boots b ON b.session_id=s.id LEFT JOIN session_integration_policy p ON p.session_id=s.id WHERE s.id=$1")
        .bind(sid.as_uuid()).fetch_optional(&mut *conn).await.map_err(db_err)?.ok_or(MetaError::NotFound)?;
    let raw: Option<String> = row.try_get("policy_json").map_err(db_err)?;
    let (retained, policy) = retain_generation(raw.as_deref(), hosts, first)?;
    if write {
        sqlx::query("INSERT INTO session_integration_policy(session_id,policy_json) VALUES($1,$2) ON CONFLICT(session_id) DO UPDATE SET policy_json=EXCLUDED.policy_json").bind(sid.as_uuid()).bind(retained).execute(&mut *conn).await.map_err(db_err)?;
    } else if raw
        .as_deref()
        .is_none_or(|r| retain(Some(r), Vec::new()).is_err())
    {
        return Err(conflict());
    }
    let boot: Option<serde_json::Value> = row.try_get("record").map_err(db_err)?;
    let boot: Option<ManagedBootRecord> = boot
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| conflict())?;
    Ok(OperatorParticipant {
        session_id: sid,
        status: row.try_get("status").map_err(db_err)?,
        host_id: row
            .try_get::<Option<uuid::Uuid>, _>("host_id")
            .map_err(db_err)?
            .map(Into::into),
        sandbox_id: row
            .try_get::<Option<uuid::Uuid>, _>("sandbox_id")
            .map_err(db_err)?
            .map(Into::into),
        binding_epoch: u64::try_from(row.try_get::<i64, _>("binding_epoch").map_err(db_err)?)
            .map_err(|_| conflict())?,
        held_creation: row
            .try_get::<Option<bool>, _>("held")
            .map_err(db_err)?
            .unwrap_or(false),
        boot: boot.map(|b| (b.request, b.revision, b.prepared)),
        policy,
    })
}
async fn lock_session(conn: &mut PgConnection, sid: SessionId) -> Result<(), MetaError> {
    sqlx::query("SELECT id FROM sessions WHERE id=$1 FOR UPDATE")
        .bind(sid.as_uuid())
        .fetch_optional(conn)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
    Ok(())
}
impl PostgresStore {
    pub(crate) async fn operator_read(&self) -> Result<OperatorPublication, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // The same snapshot includes the public gate and its immutable candidate.
        sqlx::query(
            "SELECT singleton FROM managed_operator_generation_gate WHERE singleton FOR SHARE",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let result = state(&mut tx).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(result)
    }
    pub(crate) async fn operator_stage(
        &self,
        expected: Option<u64>,
        candidate: OperatorCandidate,
    ) -> Result<OperatorPublication, MetaError> {
        candidate.identity()?;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        gate(&mut tx).await?;
        let old = state(&mut tx).await?;
        let next = old.stage(expected, candidate)?;
        if next != old {
            let pending = next.pending.as_ref().ok_or_else(conflict)?;
            sqlx::query(
                "UPDATE managed_operator_generation_gate SET admission_open=FALSE WHERE singleton",
            )
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            sqlx::query("INSERT INTO managed_operator_publications(generation,record,updated_at) VALUES($1,$2,$3)").bind(pending.generation as i64).bind(serde_json::to_value(pending).map_err(|_|conflict())?).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(next)
    }
    pub(crate) async fn operator_list(
        &self,
        after: Option<SessionId>,
        limit: u32,
    ) -> Result<Vec<SessionId>, MetaError> {
        if !(1..=512).contains(&limit) {
            return Err(conflict());
        }
        let query=format!("SELECT session_id FROM ({IDS}) p WHERE ($1::uuid IS NULL OR session_id>$1) ORDER BY session_id LIMIT $2");
        Ok(sqlx::query_scalar::<_, uuid::Uuid>(&query)
            .bind(after.map(|s| s.as_uuid()))
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(Into::into)
            .collect())
    }
    pub(crate) async fn operator_retain(
        &self,
        g: &OperatorGeneration,
        sid: SessionId,
    ) -> Result<OperatorParticipant, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        gate(&mut tx).await?;
        state(&mut tx).await?.pending(g)?;
        lock_session(&mut tx, sid).await?;
        let query = format!("SELECT EXISTS(SELECT 1 FROM ({IDS}) p WHERE session_id=$1)");
        if !sqlx::query_scalar::<_, bool>(&query)
            .bind(sid.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?
        {
            return Err(conflict());
        }
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM managed_operator_readiness WHERE generation=$1 AND session_id=$2)").bind(g.generation as i64).bind(sid.as_uuid()).fetch_one(&mut *tx).await.map_err(db_err)?;
        let p = snapshot(&mut tx, sid, g.candidate.hosts()?, true, !exists).await?;
        // Persist the requirement before host I/O. Reobserving clears transient proof.
        sqlx::query("INSERT INTO managed_operator_readiness(generation,session_id,participant,proof) VALUES($1,$2,$3,NULL) ON CONFLICT(generation,session_id) DO UPDATE SET participant=EXCLUDED.participant,proof=NULL").bind(g.generation as i64).bind(sid.as_uuid()).bind(serde_json::to_value(&p).map_err(|_|conflict())?).execute(&mut *tx).await.map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(p)
    }
    pub(crate) async fn operator_ready(
        &self,
        g: &OperatorGeneration,
        p: &OperatorParticipant,
        proof: OperatorReadiness,
    ) -> Result<(), MetaError> {
        proof.validate(g, p)?;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        gate(&mut tx).await?;
        state(&mut tx).await?.pending(g)?;
        lock_session(&mut tx, p.session_id).await?;
        if snapshot(&mut tx, p.session_id, Vec::new(), false, false).await? != *p {
            return Err(conflict());
        }
        let changed=sqlx::query("UPDATE managed_operator_readiness SET proof=$4 WHERE generation=$1 AND session_id=$2 AND participant=$3").bind(g.generation as i64).bind(p.session_id.as_uuid()).bind(serde_json::to_value(p).map_err(|_|conflict())?).bind(serde_json::to_value(proof).map_err(|_|conflict())?).execute(&mut *tx).await.map_err(db_err)?.rows_affected();
        if changed != 1 {
            return Err(conflict());
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }
    pub(crate) async fn operator_publish(
        &self,
        g: &OperatorGeneration,
    ) -> Result<OperatorPublication, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        gate(&mut tx).await?;
        let old = state(&mut tx).await?;
        if old.pending.is_none() && old.published.as_ref() == Some(g) && old.admission_open {
            return Ok(old);
        }
        old.pending(g)?;
        // Lock every participant in stable order. New reservations/claims take the gate first.
        let query=format!("SELECT s.id FROM sessions s JOIN ({IDS}) p ON p.session_id=s.id ORDER BY s.id FOR UPDATE OF s");
        let ids = sqlx::query_scalar::<_, uuid::Uuid>(&query)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
        for id in ids {
            let p = snapshot(&mut tx, id.into(), Vec::new(), false, false).await?;
            let row=sqlx::query("SELECT participant,proof FROM managed_operator_readiness WHERE generation=$1 AND session_id=$2").bind(g.generation as i64).bind(id).fetch_optional(&mut *tx).await.map_err(db_err)?.ok_or_else(conflict)?;
            let recorded: OperatorParticipant =
                serde_json::from_value(row.try_get("participant").map_err(db_err)?)
                    .map_err(|_| conflict())?;
            let proof: OperatorReadiness = serde_json::from_value(
                row.try_get::<Option<serde_json::Value>, _>("proof")
                    .map_err(db_err)?
                    .ok_or_else(conflict)?,
            )
            .map_err(|_| conflict())?;
            if p != recorded {
                return Err(conflict());
            }
            proof.validate(g, &p)?;
        }
        sqlx::query("UPDATE managed_operator_publications SET published=TRUE,updated_at=$2 WHERE generation=$1").bind(g.generation as i64).bind(self.clock.now_utc()).execute(&mut *tx).await.map_err(db_err)?;
        sqlx::query("UPDATE managed_operator_generation_gate SET published_generation=$1,published_identity=$2,admission_open=TRUE WHERE singleton").bind(g.generation as i64).bind(&g.identity).execute(&mut *tx).await.map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(OperatorPublication {
            published: Some(g.clone()),
            pending: None,
            admission_open: true,
        })
    }
}

/// New managed creations register under the same generation lock as staging.
/// Exact existing reservations are checked by their immutable create path instead.
pub(crate) async fn prepare_creation(
    conn: &mut PgConnection,
    ws: &mut engram_core::traits::SessionCreateWriteSet,
) -> Result<(), MetaError> {
    let managed_policy = has_managed_policy(ws.integration_policy_json.as_deref())?;
    if ws.managed_creation.is_none() && !managed_policy {
        return Ok(());
    }
    sqlx::query("SELECT singleton FROM managed_operator_generation_gate WHERE singleton FOR SHARE")
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM session_managed_creations WHERE session_id=$1)",
    )
    .bind(ws.session_id.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    if exists {
        return Ok(());
    }
    let publication = state(conn).await?;
    if ws.managed_creation.is_none()
        && publication.published.is_none()
        && publication.pending.is_none()
    {
        return Ok(());
    }
    if !publication.admission_open || publication.pending.is_some() {
        return Err(conflict());
    }
    let published = publication.published.ok_or_else(conflict)?;
    ws.integration_policy_json = Some(
        retain(
            ws.integration_policy_json.as_deref(),
            published.candidate.hosts()?,
        )?
        .0,
    );
    Ok(())
}
/// A newly introduced legacy managed marker cannot appear behind a pending scan.
/// Existing policies can still grow monotonically while publication is closed.
pub(crate) async fn policy_registration_lock(
    conn: &mut PgConnection,
) -> Result<OperatorPublication, MetaError> {
    sqlx::query("SELECT singleton FROM managed_operator_generation_gate WHERE singleton FOR SHARE")
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;
    state(conn).await
}
pub(crate) fn check_registration(
    publication: OperatorPublication,
    old: Option<&str>,
    next: &str,
) -> Result<(), MetaError> {
    publication.validate_registration(old, next)
}

fn has_managed_policy(raw: Option<&str>) -> Result<bool, MetaError> {
    Ok(raw
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .map_err(|_| conflict())?
        .is_some_and(|v| v.get("managed_deny").is_some_and(|p| !p.is_null())))
}
